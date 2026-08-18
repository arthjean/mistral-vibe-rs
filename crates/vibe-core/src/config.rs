use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::{Value as JsonValue, json};
use thiserror::Error;
use toml::{Table, Value};
use url::Url;

use crate::atomic_file::AtomicWriteError;
use crate::mcp::{
    DEFAULT_MCP_STARTUP_TIMEOUT_MS, DEFAULT_MCP_TOOL_TIMEOUT_MS, McpServerConfig,
    McpTransportConfig,
};
use crate::redaction::{is_sensitive_key, redact_table, redact_value};
use document::{
    fingerprint_optional, hex_digest, migrate_file, patch_target_document, persist_models_as_list,
    read_table_optional, validate_table,
};
use effective::{finalize_effective, model_order, require_configured_model};
use environment::environment_table;
use transaction::{
    ConfigFileLock, ConfigJournal, JournalState, PreparedWrite, cleanup_orphan_sidecars,
    cleanup_prepared, commit_prepared, ensure_private_directory, recover_transaction,
    rollback_prepared, sync_directory, write_journal,
};

mod document;
pub mod dotenv;
mod effective;
mod environment;
pub mod events;
pub mod experiments_layer;
pub mod harness;
pub mod introspect;
mod merge;
pub mod migration;
pub mod patch;
mod providers;
mod proxy;
pub mod registry;
mod transaction;
mod view;

pub use dotenv::{DotenvValues, global_env_file};
pub(crate) use effective::user_home_directory;
pub use effective::{active_model_alias, default_model_alias};
pub use events::{ConfigChangeBus, ConfigChangeEvent, ConfigSubscription};
pub use experiments_layer::{
    EXPERIMENTS_LAYER_NAME, ExperimentsLayer, PromptResolves, configured_fields,
};
pub use harness::{ConfigSource, HarnessFiles};
pub use introspect::{ConfigFieldView, ConfigFields, ConfigLayerValue, HIDDEN_FIELDS};
pub use patch::{ConfigMutation, JsonPointer, PatchError, PatchOperation};
pub use proxy::{ProxyEnvironmentStore, ProxyKey, ProxyKeyError};

/// The alias new turns run on, empty where the operator pinned nothing.
const ACTIVE_MODEL_FIELD: &str = "active_model";
/// The alias an experiment routes an unpinned installation onto.
const ROUTED_DEFAULT_MODEL_FIELD: &str = "routed_default_model";
/// The definition of that alias, carried by the same experiment.
const ROUTED_MODEL_CONFIG_FIELD: &str = "routed_model_config";

const CONFIG_FILE: &str = "config.toml";
const PROJECT_DIRECTORY: &str = ".vibe";
const TRANSACTION_FILE: &str = ".config-transaction.json";
const LOCK_FILE: &str = ".config.lock";
const MAX_CONFIG_BYTES: u64 = 4 * 1024 * 1024;

/// Every accepted `theme` value: the reference `sorted_theme_names()` catalog
/// plus the native polarity values this port already persisted.
pub const THEME_VALUES: [&str; 25] = [
    "system",
    "light",
    "dark",
    "auto",
    "ansi-light",
    "atom-one-light",
    "catppuccin-latte",
    "rose-pine-dawn",
    "solarized-light",
    "textual-light",
    "ansi-dark",
    "atom-one-dark",
    "catppuccin-frappe",
    "catppuccin-macchiato",
    "catppuccin-mocha",
    "dracula",
    "flexoki",
    "gruvbox",
    "monokai",
    "nord",
    "rose-pine",
    "rose-pine-moon",
    "solarized-dark",
    "textual-dark",
    "tokyo-night",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigLayerKind {
    Defaults,
    /// What a runtime discovery pass contributes, chiefly the per-tool settings
    /// the published tools declare. Reference `DiscoveredConfigLayer`, whose
    /// position `build_default_orchestrator` documents as sitting between the
    /// schema defaults and the selected file, so every file an operator owns
    /// overrides it.
    Discovered,
    /// What a resolved rollout assigns, composed from
    /// [`experiments_layer::ExperimentsLayer`]. Reference
    /// `build_default_orchestrator`, which seats `GrowthbookLayer` directly
    /// above the schema defaults, so a value written in any file an operator
    /// owns beats an assignment.
    Experiments,
    SelectedToml,
    Environment,
    Runtime,
    Agent,
}

/// A runtime discovery pass, run once per [`LayeredConfig::load`].
///
/// It answers with the document its layer composes, or with the reason it could
/// not: a failed pass empties the layer and is reported as a validation
/// warning rather than failing the load, because a configuration must stay
/// readable when the thing it describes cannot be enumerated.
pub type ConfigDiscovery = Arc<dyn Fn() -> Result<Table, String> + Send + Sync>;

/// A component told about every snapshot a load composes.
///
/// The change bus reports what a *write through this process* moved, which is
/// blind to a file an operator edits by hand. An observer sees the composed
/// document instead, so a component caching part of it stays current whatever
/// moved it.
pub type ConfigObserver = Arc<dyn Fn(&ConfigSnapshot) + Send + Sync>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigTarget {
    User,
    Project,
    /// No enabled source resolves to a file: the selection is held in memory
    /// for the life of the store and no write reaches disk. Reference
    /// `_select_persistence_layer`, whose last fallback is an ephemeral layer.
    Ephemeral,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ConfigLayer {
    pub kind: ConfigLayerKind,
    pub values: Table,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigPaths {
    pub vibe_home: PathBuf,
    pub working_directory: PathBuf,
}

impl ConfigPaths {
    #[must_use]
    pub fn user_config(&self) -> PathBuf {
        self.vibe_home.join(CONFIG_FILE)
    }

    /// The project file governing the working directory: the nearest one found
    /// by walking up, and the working directory's own path when the walk finds
    /// nothing. Reference `ProjectConfigLayer._target_path`, which writes to the
    /// discovered file and falls back on the undiscovered one.
    #[must_use]
    pub fn project_config(&self) -> PathBuf {
        self.discovered_project_config()
            .unwrap_or_else(|| self.local_project_config())
    }

    /// The project file the walk found, or `None` when no directory between the
    /// working directory and the vibe home holds one.
    #[must_use]
    pub fn discovered_project_config(&self) -> Option<PathBuf> {
        harness::discover_project_config(&self.working_directory, &self.vibe_home)
    }

    /// The project file the working directory would carry itself, whether or
    /// not it exists.
    #[must_use]
    pub fn local_project_config(&self) -> PathBuf {
        self.working_directory
            .join(PROJECT_DIRECTORY)
            .join(CONFIG_FILE)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigSnapshot {
    pub effective: Table,
    pub selected_target: ConfigTarget,
    pub selected_path: PathBuf,
    pub fingerprints: BTreeMap<ConfigTarget, Option<String>>,
    pub target_values: BTreeMap<ConfigTarget, Table>,
    pub layer_values: Vec<ConfigLayer>,
    /// What the load repaired rather than rejected, in the order the repairs
    /// were made. Reference `VibeConfigSchema.validation_warnings`.
    pub validation_warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IntegrationPreference {
    pub enabled: bool,
    pub disabled_tools: BTreeSet<String>,
}

impl ConfigSnapshot {
    /// The alias new turns run on, with the unpinned sentinel resolved.
    ///
    /// The merged document keeps the sentinel, so anything selecting a model
    /// reads it through here rather than through `active_model` directly.
    #[must_use]
    pub fn active_model_alias(&self) -> Option<&str> {
        active_model_alias(&self.effective)
    }

    #[must_use]
    pub fn public_view(&self) -> JsonValue {
        json!({
            "config": redact_table(&self.effective),
            "selectedTarget": self.selected_target,
            "selectedPath": self.selected_path,
            "fingerprints": self.fingerprints,
            "targetValues": self.target_values.iter().map(|(target, values)| {
                (target, redact_table(values))
            }).collect::<BTreeMap<_, _>>(),
            "layers": self.layer_values.iter().map(|layer| layer.kind).collect::<Vec<_>>(),
            "layerValues": self.layer_values.iter().map(|layer| json!({
                "layer": layer.kind,
                "values": redact_table(&layer.values),
            })).collect::<Vec<_>>(),
            // What the load repaired rather than rejected, so a client can say
            // so instead of silently running on a different model.
            "validationWarnings": self.validation_warnings,
        })
    }

    #[must_use]
    pub fn public_value(&self, key: &str) -> JsonValue {
        self.effective
            .get(key)
            .map(|value| {
                if is_sensitive_key(key) {
                    JsonValue::String("[redacted]".to_owned())
                } else {
                    redact_value(value)
                }
            })
            .unwrap_or(JsonValue::Null)
    }

    /// The effective keys [`registry::FIELDS`] does not declare, in document
    /// order.
    ///
    /// The reference merge drops a key its schema does not know; this port keeps
    /// it, so a key persisted by a newer client survives a round trip through
    /// this one. This accessor is how that set is reported rather than silently
    /// carried.
    #[must_use]
    pub fn unregistered_keys(&self) -> Vec<&str> {
        self.effective
            .keys()
            .map(String::as_str)
            .filter(|key| registry::field(key).is_none())
            .collect()
    }

    /// The `enabled_tools` allowlist the configuration carries.
    ///
    /// Reference `VibeConfigSchema.enabled_tools`: when it holds an entry, only
    /// the tools it matches are published. Entries are glob or `re:` patterns,
    /// matched by [`crate::matching::NameFilter`].
    #[must_use]
    pub fn enabled_tools(&self) -> Vec<String> {
        self.string_array("enabled_tools")
    }

    /// The `disabled_tools` denylist the configuration carries, applied after
    /// the allowlist as reference `available_tools` applies it.
    #[must_use]
    pub fn disabled_tools(&self) -> Vec<String> {
        self.string_array("disabled_tools")
    }

    /// The extra skill directories the configuration names.
    ///
    /// Reference `VibeConfigSchema.skill_paths`: entries are concatenated
    /// across layers and expanded by `_expand_paths`. They stay strings here
    /// because expansion needs the home and the working directory, which
    /// [`crate::skills::search_paths`] is the one place that knows.
    #[must_use]
    pub fn skill_paths(&self) -> Vec<String> {
        self.string_array("skill_paths")
    }

    /// The `enabled_skills` allowlist. When it holds an entry, only the skills
    /// it matches are published and `disabled_skills` is ignored.
    #[must_use]
    pub fn enabled_skills(&self) -> Vec<String> {
        self.string_array("enabled_skills")
    }

    /// The `disabled_skills` denylist, consulted only when `enabled_skills`
    /// holds nothing.
    #[must_use]
    pub fn disabled_skills(&self) -> Vec<String> {
        self.string_array("disabled_skills")
    }

    /// Whether the registry-skills experiment is enabled.
    ///
    /// Reference `experimental_enable_registry_skills` has exactly one
    /// occurrence upstream, its own declaration; here the key gates the ported
    /// registry subtree, which stays dormant behind it.
    #[must_use]
    pub fn registry_skills_enabled(&self) -> bool {
        self.effective
            .get("experimental_enable_registry_skills")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }

    /// The string entries of a top-level array key, skipping anything that is
    /// not a string so one mistyped entry cannot take the session down.
    fn string_array(&self, key: &str) -> Vec<String> {
        self.effective
            .get(key)
            .and_then(Value::as_array)
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(|entry| entry.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn mcp_servers(
        &self,
        working_directory: &Path,
    ) -> Result<Vec<McpServerConfig>, ConfigError> {
        let Some(value) = self.effective.get("mcp_servers") else {
            return Ok(Vec::new());
        };
        let entries = value.as_array().ok_or_else(|| {
            ConfigError::InvalidMcp("mcp_servers must be an array of tables".to_owned())
        })?;
        let mut aliases = BTreeSet::new();
        let mut servers = Vec::new();
        for entry in entries {
            let table = entry.as_table().ok_or_else(|| {
                ConfigError::InvalidMcp("each mcp_servers entry must be a table".to_owned())
            })?;
            let server = decode_mcp_server(table, working_directory)?;
            if !aliases.insert(server.alias.clone()) {
                return Err(ConfigError::InvalidMcp(
                    "MCP server names must be unique".to_owned(),
                ));
            }
            servers.push(server);
        }
        Ok(servers)
    }

    /// The alias of every configured server, under the name a reader sees.
    pub fn mcp_aliases(&self) -> Result<BTreeSet<String>, ConfigError> {
        config_array(&self.effective, IntegrationCollection::McpServers)?
            .into_iter()
            .map(|entry| {
                entry
                    .as_table()
                    .and_then(|entry| IntegrationCollection::McpServers.identity_key(entry))
                    .filter(|name| !name.is_empty())
                    .ok_or_else(|| {
                        ConfigError::InvalidMcp(
                            "each effective mcp_servers entry requires a non-empty name".to_owned(),
                        )
                    })
            })
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq)]

pub struct ConfigWrite {
    pub target: ConfigTarget,
    pub expected_fingerprint: Option<String>,
    pub mutations: Vec<ConfigMutation>,
}

/// One operation of a `config/patch` request: an addressed change, and the file
/// it goes to when the client picked one.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigPatchOp {
    pub mutation: ConfigMutation,
    /// `None` routes the operation to the target the current selection resolves
    /// to, as the reference routes an operation carrying no `target_layer`.
    pub target: Option<ConfigTarget>,
}

/// What applying a patch did.
///
/// Writes are not atomic across targets: the preflight rejects the whole
/// request, and past that point each target is written independently and its
/// failure is reported rather than raised. Reference
/// `ConfigOrchestrator.apply_patch`.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigPatchOutcome {
    /// The configuration as it stands after the patch.
    pub snapshot: ConfigSnapshot,
    /// One entry per target whose write failed, in target order.
    pub failures: Vec<String>,
}

#[derive(Clone)]
pub struct LayeredConfig {
    paths: ConfigPaths,
    defaults: Table,
    /// The pass behind [`ConfigLayerKind::Discovered`], or `None` when nothing
    /// discovers anything for this store, which leaves the layer empty.
    discovery: Option<ConfigDiscovery>,
    /// The document behind [`ConfigLayerKind::Experiments`], shared by every
    /// clone of this store because a rollout resolves after a session is
    /// already running: reference `_sync_growthbook_layer_variants` writes into
    /// the orchestrator's own layer object, and every reader that had already
    /// taken a handle composes with what it wrote.
    experiments: Arc<Mutex<Table>>,
    runtime: Table,
    agent: Table,
    environment: BTreeMap<String, String>,
    project_trusted: bool,
    sources: BTreeSet<ConfigSource>,
    additional_roots: Vec<PathBuf>,
    /// The document behind [`ConfigTarget::Ephemeral`], shared by every clone of
    /// this store so a write made through one is read back through another.
    ephemeral: Arc<Mutex<Table>>,
    migrated: Arc<Mutex<bool>>,
    /// What [`Self::migrate_sources`] could not write, carried by every
    /// snapshot the store produces afterward.
    migration_warnings: Arc<Mutex<Vec<String>>>,
    transaction_lock: Arc<Mutex<()>>,
    events: Arc<ConfigChangeBus>,
    /// Shared by every clone, so a component that attached to one handle hears
    /// about a load made through another.
    observers: Arc<Mutex<Vec<ConfigObserver>>>,
}

impl LayeredConfig {
    #[must_use]
    pub fn new(paths: ConfigPaths, defaults: Table) -> Self {
        Self {
            paths,
            defaults,
            discovery: None,
            experiments: Arc::new(Mutex::new(Table::new())),
            runtime: Table::new(),
            agent: Table::new(),
            environment: BTreeMap::new(),
            project_trusted: false,
            sources: ConfigSource::all(),
            additional_roots: Vec::new(),
            ephemeral: Arc::new(Mutex::new(Table::new())),
            migrated: Arc::new(Mutex::new(false)),
            migration_warnings: Arc::new(Mutex::new(Vec::new())),
            transaction_lock: Arc::new(Mutex::new(())),
            events: Arc::new(ConfigChangeBus::default()),
            observers: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Registers `observer` for every snapshot [`Self::load`] composes.
    ///
    /// A load is what every reader already goes through, so this is where a
    /// cache of part of the document is refreshed: it covers a file edited
    /// outside this process, which the change bus never reports.
    pub fn observe(&self, observer: ConfigObserver) {
        if let Ok(mut observers) = self.observers.lock() {
            observers.push(observer);
        }
    }

    /// Restricts the store to the configuration sources `sources` enables.
    ///
    /// Both are enabled by default, which is what every binary in this
    /// workspace opens with. Dropping [`ConfigSource::User`] refuses every
    /// write to the user file, matching the reference `persist_allowed` rule.
    #[must_use]
    pub fn with_sources(mut self, sources: BTreeSet<ConfigSource>) -> Self {
        self.sources = sources;
        self
    }

    /// Adds the directories opened alongside the working directory, as
    /// `--add-dir` does upstream.
    #[must_use]
    pub fn with_additional_roots(mut self, roots: Vec<PathBuf>) -> Self {
        self.additional_roots = roots;
        self
    }

    /// The file resolution behind this store: the selected file, the open
    /// project roots and whether a write may persist.
    #[must_use]
    pub fn harness_files(&self) -> HarnessFiles {
        HarnessFiles::new(
            self.paths.clone(),
            self.sources.clone(),
            self.additional_roots.clone(),
            self.project_trusted,
        )
    }

    /// Registers `callback` for the configuration keys it names, or for every
    /// change when it names none, and returns the handle that cancels it.
    ///
    /// The bus is shared by every clone of this store, including the
    /// working-directory scoped ones, so a subscriber registered once hears
    /// about a write made through any of them.
    pub fn subscribe(
        &self,
        keys: Option<BTreeSet<String>>,
        callback: impl Fn(&ConfigChangeEvent) + Send + Sync + 'static,
    ) -> ConfigSubscription {
        self.events.subscribe(keys, callback)
    }

    /// Installs the pass composing [`ConfigLayerKind::Discovered`].
    ///
    /// The pass runs on every load, because what it enumerates is a property of
    /// the running process rather than of a file: a tool that becomes available
    /// between two loads belongs in the second snapshot.
    #[must_use]
    pub fn with_discovery(mut self, discovery: ConfigDiscovery) -> Self {
        self.discovery = Some(discovery);
        self
    }

    #[must_use]
    pub fn with_experiments(self, values: Table) -> Self {
        self.set_experiments(values);
        self
    }

    /// Publishes what [`ConfigLayerKind::Experiments`] composes, for every
    /// handle onto this store.
    ///
    /// This is the write a resolved rollout makes, and it is a write rather
    /// than a builder because the assignment arrives after every consumer has
    /// already taken its handle. The next [`Self::load`] composes it, which is
    /// what carries it to the caches that follow a load.
    pub fn set_experiments(&self, values: Table) {
        *self
            .experiments
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = values;
    }

    /// Publishes the variants a resolved rollout assigned, mapped onto the
    /// fields the layer writes.
    ///
    /// Reference `_sync_growthbook_layer_variants`.
    pub fn set_experiment_variants(
        &self,
        variants: &BTreeMap<String, String>,
        prompt_resolves: PromptResolves,
    ) {
        self.set_experiments(
            ExperimentsLayer::from_variants(variants, prompt_resolves).into_values(),
        );
    }

    /// Installs the document `variants` map onto, which is how a resolved
    /// rollout reaches [`ConfigLayerKind::Experiments`].
    ///
    /// The variants are the ones
    /// [`crate::experiments::ExperimentManager::config_variants`] answers, and
    /// `prompt_resolves` is the session's own prompt resolution, which decides
    /// whether a prompt variant is written at all.
    #[must_use]
    pub fn with_experiment_variants(
        self,
        variants: &BTreeMap<String, String>,
        prompt_resolves: PromptResolves,
    ) -> Self {
        self.set_experiment_variants(variants, prompt_resolves);
        self
    }

    #[must_use]
    pub fn with_runtime_overrides(mut self, values: Table) -> Self {
        self.runtime = values;
        self
    }

    #[must_use]
    pub fn with_agent_overlay(mut self, values: Table) -> Self {
        self.agent = values;
        self
    }

    #[must_use]
    pub fn with_environment(mut self, values: impl IntoIterator<Item = (String, String)>) -> Self {
        self.environment = values.into_iter().collect();
        self
    }

    #[must_use]
    pub fn with_project_trusted(mut self, trusted: bool) -> Self {
        self.project_trusted = trusted;
        self
    }

    #[must_use]
    pub fn scoped_to_working_directory(
        &self,
        working_directory: PathBuf,
        project_trusted: bool,
    ) -> Self {
        let mut scoped = self.clone();
        scoped.paths.working_directory = working_directory;
        scoped.project_trusted = project_trusted;
        scoped
    }

    /// The document [`ConfigLayerKind::Discovered`] composes, and the single
    /// warning a failed pass leaves behind.
    ///
    /// A store with no pass installed, and a pass that answers with nothing,
    /// are the same thing here: an empty layer that leaves the effective
    /// document as the other layers composed it.
    fn discovered_layer(&self) -> (Table, Option<String>) {
        match self.discovery.as_ref().map(|discovery| discovery()) {
            None => (Table::new(), None),
            Some(Ok(values)) => (values, None),
            Some(Err(reason)) => (
                Table::new(),
                Some(format!("Runtime discovery is unavailable: {reason}")),
            ),
        }
    }

    pub fn load(&self) -> Result<ConfigSnapshot, ConfigError> {
        let _guard = self
            .transaction_lock
            .lock()
            .map_err(|_| ConfigError::LockPoisoned)?;
        ensure_private_directory(&self.paths.vibe_home)?;
        let _file_guard = ConfigFileLock::acquire(&self.paths.vibe_home)?;
        recover_transaction(&self.paths)?;
        cleanup_orphan_sidecars(&self.paths)?;

        let harness = self.harness_files();
        let user_path = self.paths.user_config();
        let project_path = self.paths.project_config();
        // The selection is the file the enabled sources resolve to: the
        // discovered project file while the workspace is trusted, the user file
        // otherwise, and the in-memory document when neither source answers.
        let (selected_target, selected_path) = harness
            .config_file()
            .unwrap_or((ConfigTarget::Ephemeral, PathBuf::new()));

        let user_values = if self.sources.contains(&ConfigSource::User) {
            read_table_optional(&user_path)?
        } else {
            Table::new()
        };
        let project_values =
            if self.project_trusted && self.sources.contains(&ConfigSource::Project) {
                read_table_optional(&project_path)?
            } else {
                Table::new()
            };
        let mut target_values = BTreeMap::from([
            (ConfigTarget::User, user_values),
            (ConfigTarget::Project, project_values),
        ]);
        if selected_target == ConfigTarget::Ephemeral {
            target_values.insert(ConfigTarget::Ephemeral, self.ephemeral_document()?);
        }
        let selected = target_values
            .get(&selected_target)
            .cloned()
            .unwrap_or_default();
        let environment = environment_table(&self.environment)?;
        let (discovered, discovery_failure) = self.discovered_layer();
        let layers = vec![
            ConfigLayer {
                kind: ConfigLayerKind::Defaults,
                values: self.defaults.clone(),
            },
            ConfigLayer {
                kind: ConfigLayerKind::Discovered,
                values: discovered,
            },
            // The experiment assignment composes below the selected file:
            // reference `build_default_orchestrator` seats `GrowthbookLayer`
            // directly above the schema defaults so that every file an operator
            // owns overrides an assignment. Enrollment is still reported to
            // telemetry, but it never silently overrides a value a human wrote.
            ConfigLayer {
                kind: ConfigLayerKind::Experiments,
                values: self
                    .experiments
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone(),
            },
            ConfigLayer {
                kind: ConfigLayerKind::SelectedToml,
                values: selected.clone(),
            },
            ConfigLayer {
                kind: ConfigLayerKind::Environment,
                values: environment,
            },
            ConfigLayer {
                kind: ConfigLayerKind::Runtime,
                values: self.runtime.clone(),
            },
            ConfigLayer {
                kind: ConfigLayerKind::Agent,
                values: self.agent.clone(),
            },
        ];
        let mut effective = Table::new();
        for layer in &layers {
            merge_layer(&mut effective, &layer.values)?;
        }
        validate_table(&effective)?;
        // What a migration could not write is reported by every snapshot that
        // follows it, so a client reading the configuration sees why its file
        // still carries the old shape.
        let mut validation_warnings = self
            .migration_warnings
            .lock()
            .map_err(|_| ConfigError::LockPoisoned)?
            .clone();
        // One entry, whatever the pass was enumerating: the snapshot reports
        // that the discovered layer is empty and why, not every item it failed
        // to reach.
        validation_warnings.extend(discovery_failure);
        validation_warnings.extend(finalize_effective(
            &mut effective,
            &self.paths.vibe_home,
            &model_order(&layers),
        )?);

        let mut fingerprints = BTreeMap::from([
            (
                ConfigTarget::User,
                fingerprint_optional(&self.paths.user_config())?,
            ),
            (ConfigTarget::Project, fingerprint_optional(&project_path)?),
        ]);
        if selected_target == ConfigTarget::Ephemeral {
            // The in-memory document has no file to stat, so its fingerprint is
            // taken over the document itself; a concurrent-edit check against it
            // still compares what the caller last saw.
            fingerprints.insert(
                ConfigTarget::Ephemeral,
                Some(hex_digest(selected.to_string().as_bytes())),
            );
        }
        let snapshot = ConfigSnapshot {
            effective,
            selected_target,
            selected_path,
            fingerprints,
            target_values,
            layer_values: layers,
            validation_warnings,
        };
        // The observers run under the same guards the load holds, which is what
        // keeps a cache from reading a document another load is replacing. None
        // of them writes configuration, so the lock cannot come back around.
        if let Ok(observers) = self.observers.lock() {
            for observer in observers.iter() {
                observer(&snapshot);
            }
        }
        Ok(snapshot)
    }

    pub fn batch_write(&self, writes: &[ConfigWrite]) -> Result<ConfigSnapshot, ConfigError> {
        if writes.is_empty() {
            return self.load();
        }
        let _guard = self
            .transaction_lock
            .lock()
            .map_err(|_| ConfigError::LockPoisoned)?;
        ensure_private_directory(&self.paths.vibe_home)?;
        let _file_guard = ConfigFileLock::acquire(&self.paths.vibe_home)?;
        recover_transaction(&self.paths)?;
        cleanup_orphan_sidecars(&self.paths)?;

        let mut targets = BTreeSet::new();
        let mut prepared = Vec::with_capacity(writes.len());
        let mut ephemeral = None;
        for write in writes {
            if !targets.insert(write.target) {
                return Err(ConfigError::DuplicateTarget(write.target));
            }
            if write.target == ConfigTarget::Project && !self.project_trusted {
                return Err(ConfigError::UntrustedProject);
            }
            // A source the session did not enable is never written to, which is
            // how `persist_allowed` refuses a user write when only the project
            // source is open.
            if let Some(source) = source_of(write.target)
                && !self.sources.contains(&source)
            {
                return Err(ConfigError::PersistenceDisabled(write.target));
            }
            let Some(path) = self.target_path(write.target) else {
                let persisted = self.ephemeral_document()?;
                if Some(hex_digest(persisted.to_string().as_bytes())) != write.expected_fingerprint
                {
                    return Err(ConfigError::ConcurrentEdit {
                        target: write.target,
                    });
                }
                let table = patch_target_document(&persisted, &write.mutations)?;
                validate_table(&table)?;
                ephemeral = Some(table);
                continue;
            };
            let actual_fingerprint = fingerprint_optional(&path)?;
            if actual_fingerprint != write.expected_fingerprint {
                return Err(ConfigError::ConcurrentEdit {
                    target: write.target,
                });
            }
            let persisted = read_table_optional(&path)?;
            let mut table = patch_target_document(&persisted, &write.mutations)?;
            validate_table(&table)?;
            persist_models_as_list(&mut table, &merge::persisted_model_order(&persisted));
            let encoded = toml::to_string_pretty(&table).map_err(ConfigError::Serialize)?;
            prepared.push(PreparedWrite::new(path, encoded.into_bytes())?);
        }

        if prepared.is_empty() {
            // Nothing to journal: the write never leaves memory.
            if let Some(table) = ephemeral {
                *self
                    .ephemeral
                    .lock()
                    .map_err(|_| ConfigError::LockPoisoned)? = table;
            }
            drop(_file_guard);
            drop(_guard);
            return self.load();
        }

        let journal_path = self.paths.vibe_home.join(TRANSACTION_FILE);
        let mut journal = ConfigJournal {
            state: JournalState::Prepared,
            entries: prepared.iter().map(PreparedWrite::journal_entry).collect(),
        };
        write_journal(&journal_path, &journal)?;
        for item in &mut prepared {
            item.cleanup_on_drop = false;
        }

        if let Err(error) = commit_prepared(&prepared) {
            if let Err(rollback) = rollback_prepared(&prepared) {
                return Err(ConfigError::RollbackFailed {
                    commit: error.to_string(),
                    rollback: rollback.to_string(),
                });
            }
            fs::remove_file(&journal_path).map_err(|source| ConfigError::Io {
                path: journal_path.clone(),
                source,
            })?;
            return Err(error);
        }
        journal.state = JournalState::Committed;
        write_journal(&journal_path, &journal)?;
        cleanup_prepared(&prepared)?;
        fs::remove_file(&journal_path).map_err(|source| ConfigError::Io {
            path: journal_path,
            source,
        })?;
        sync_directory(&self.paths.vibe_home)?;
        if let Some(table) = ephemeral {
            *self
                .ephemeral
                .lock()
                .map_err(|_| ConfigError::LockPoisoned)? = table;
        }
        drop(_file_guard);
        drop(_guard);
        self.load()
    }

    /// Brings every writable configuration file forward, once per store.
    ///
    /// This is a startup step, not part of composing a configuration: the
    /// reference migrates its layers in `build_default_orchestrator` while
    /// `ConfigBuilder.build` merges untouched documents, and the committed
    /// corpus records the unmigrated merge. [`Self::load`] therefore never
    /// calls this; the binaries do, once, before their first read.
    ///
    /// A source the session did not enable, or a project file the workspace
    /// does not trust, is never rewritten, and the load then reads the
    /// unmigrated document. A write that fails is reported as a warning, left
    /// on every later snapshot, and leaves the original file intact.
    pub fn migrate_sources(&self) -> Result<Vec<String>, ConfigError> {
        let mut migrated = self
            .migrated
            .lock()
            .map_err(|_| ConfigError::LockPoisoned)?;
        if *migrated {
            return Ok(self
                .migration_warnings
                .lock()
                .map_err(|_| ConfigError::LockPoisoned)?
                .clone());
        }
        *migrated = true;
        let _guard = self
            .transaction_lock
            .lock()
            .map_err(|_| ConfigError::LockPoisoned)?;
        ensure_private_directory(&self.paths.vibe_home)?;
        let _file_guard = ConfigFileLock::acquire(&self.paths.vibe_home)?;

        let harness = self.harness_files();
        let mut files = Vec::new();
        if harness.persist_allowed() {
            files.push(self.paths.user_config());
        }
        if let Some(path) = harness.trusted_project_config() {
            files.push(path);
        }
        let mut warnings = Vec::new();
        for path in files {
            warnings.extend(migrate_file(&path)?);
        }
        self.migration_warnings
            .lock()
            .map_err(|_| ConfigError::LockPoisoned)?
            .clone_from(&warnings);
        Ok(warnings)
    }

    /// The in-memory document behind [`ConfigTarget::Ephemeral`].
    fn ephemeral_document(&self) -> Result<Table, ConfigError> {
        Ok(self
            .ephemeral
            .lock()
            .map_err(|_| ConfigError::LockPoisoned)?
            .clone())
    }

    /// Applies addressed operations to the files that back them.
    ///
    /// The merged-configuration preflight runs first and rejects the whole
    /// request, leaving every file byte-identical. Past it the operations are
    /// grouped by target and each group is written on its own, so one target
    /// failing is reported in [`ConfigPatchOutcome::failures`] rather than
    /// undoing the group that succeeded. Reference
    /// `ConfigOrchestrator.apply_patch`.
    ///
    /// A change to the effective document is published to the subscribers
    /// [`Self::subscribe`] registered; a patch that writes the value already in
    /// place publishes nothing.
    pub fn apply_patch(
        &self,
        operations: &[ConfigPatchOp],
        reason: &str,
    ) -> Result<ConfigPatchOutcome, ConfigError> {
        let before = self.load()?;
        if operations.is_empty() {
            return Ok(ConfigPatchOutcome {
                snapshot: before,
                failures: Vec::new(),
            });
        }
        let mutations: Vec<ConfigMutation> = operations
            .iter()
            .map(|operation| operation.mutation.clone())
            .collect();
        // The preflight patches the merged document, which carries models as
        // the alias-keyed map a pointer addresses them through. Every way it can
        // fail is a rejection of the whole request, as the reference rejects on
        // a pointer error and on a validation error alike.
        let reject = |error: &dyn std::fmt::Display| ConfigError::PatchRejected(error.to_string());
        let simulated =
            patch::apply_all(&before.effective, &mutations).map_err(|error| reject(&error))?;
        validate_table(&simulated).map_err(|error| reject(&error))?;
        require_configured_model(&simulated).map_err(|error| reject(&error))?;

        let mut grouped: BTreeMap<ConfigTarget, Vec<ConfigMutation>> = BTreeMap::new();
        for operation in operations {
            let target = operation.target.unwrap_or(before.selected_target);
            // Refused before anything is written, so a revoked workspace cannot
            // leave half a patch on disk.
            if target == ConfigTarget::Project && !self.project_trusted {
                return Err(ConfigError::UntrustedProject);
            }
            grouped
                .entry(target)
                .or_default()
                .push(operation.mutation.clone());
        }

        let target_count = grouped.len();
        let mut failures = Vec::new();
        for (target, mutations) in grouped {
            let write = ConfigWrite {
                target,
                expected_fingerprint: before.fingerprints.get(&target).cloned().flatten(),
                mutations,
            };
            if let Err(error) = self.batch_write(&[write]) {
                failures.push(error.to_string());
            }
        }

        let after = self.load()?;
        // The diff runs on the documents as they are, and only the payload is
        // redacted: two different secrets both read `[redacted]`, so diffing the
        // redacted forms would hide a change between them.
        let changed_keys = events::changed_keys(
            &serde_json::to_value(&before.effective).map_err(ConfigError::Json)?,
            &serde_json::to_value(&after.effective).map_err(ConfigError::Json)?,
        );
        if failures.len() < target_count && !changed_keys.is_empty() {
            self.events.publish(&ConfigChangeEvent {
                changed_keys,
                before: redact_table(&before.effective),
                after: redact_table(&after.effective),
                reason: reason.to_owned(),
            });
        }
        Ok(ConfigPatchOutcome {
            snapshot: after,
            failures,
        })
    }

    /// The settings surface: every published field with its per-layer values,
    /// and the targets a write can be routed to.
    pub fn describe_fields(&self) -> Result<ConfigFields, ConfigError> {
        let snapshot = self.load()?;
        let targets = self.writable_targets(&snapshot);
        Ok(ConfigFields {
            fields: introspect::describe_fields(&snapshot),
            targets,
        })
    }

    /// The configuration files a write can land in, the one an unrouted
    /// operation goes to first.
    ///
    /// The project file is only writable while the workspace is trusted, which
    /// is the same rule [`Self::batch_write`] enforces.
    fn writable_targets(&self, snapshot: &ConfigSnapshot) -> Vec<ConfigTarget> {
        let mut targets = vec![snapshot.selected_target];
        for target in [ConfigTarget::User, ConfigTarget::Project] {
            let enabled = source_of(target).is_some_and(|source| self.sources.contains(&source));
            let writable = enabled && (target != ConfigTarget::Project || self.project_trusted);
            if writable && !targets.contains(&target) {
                targets.push(target);
            }
        }
        targets
    }

    /// The JSON Schema for the published configuration surface, generated from
    /// [`registry::FIELDS`].
    #[must_use]
    pub fn schema() -> JsonValue {
        registry::json_schema()
    }

    /// The token identifying [`Self::schema`], stable for the life of the
    /// process so a client can cache the surface it names.
    #[must_use]
    pub fn schema_version() -> &'static str {
        registry::schema_version()
    }

    /// The schema literal this port published before the registry generated
    /// it, kept whole as the fixture `registry_tests` diffs against so the
    /// generated surface is proved unchanged rather than assumed.
    #[cfg(test)]
    fn schema_before_the_registry() -> JsonValue {
        json!({
            "type": "object",
            "additionalProperties": true,
            "properties": {
                "active_model": {"type": "string", "default": "", "description": "Model used for new turns."},
                "thinking": {"enum": ["off", "low", "medium", "high", "max"], "default": "off", "description": "Reasoning effort for the active model."},
                "theme": {"enum": THEME_VALUES, "default": "system", "description": "Terminal color theme."},
                "notifications": {
                    "enum": ["off", "unfocused", "always"],
                    "default": "unfocused",
                    "description": "When desktop notifications may be sent."
                },
                "enable_update_checks": {"type": "boolean", "default": true, "description": "Check for new releases in the background."},
                "show_thinking_nodes": {"type": "boolean", "default": true, "description": "Show reasoning regions in the transcript."},
                "voice_mode_enabled": {"type": "boolean", "default": false, "description": "Enable voice input."},
                "narrator_enabled": {"type": "boolean", "default": false, "description": "Read eligible assistant responses aloud."},
                "proxy": {"type": ["string", "null"], "format": "uri", "description": "Legacy proxy URL. Prefer /proxy-setup for protocol-specific values."},
                "tls_ca_path": {"type": ["string", "null"], "description": "Legacy TLS certificate path. Prefer /proxy-setup."},
                "dotenv_path": {"type": ["string", "null"], "description": "Optional dotenv file loaded by the runtime."},
                "enabled_tools": {
                    "type": "array",
                    "items": {"type": "string"},
                    "default": [],
                    "description": "Tool names or patterns to publish. When set, only matching tools are published. Globs and `re:` regular expressions are supported."
                },
                "disabled_tools": {
                    "type": "array",
                    "items": {"type": "string"},
                    "default": [],
                    "description": "Tool names or patterns to withhold, applied after `enabled_tools`. Globs and `re:` regular expressions are supported."
                },
                "mcp_servers": {
                    "type": "array",
                    "description": "MCP server definitions.",
                    "items": {
                        "type": "object",
                        "additionalProperties": false,
                        "required": ["name", "transport"],
                        "properties": {
                            "name": {"type": "string", "minLength": 1},
                            "transport": {"enum": ["stdio", "streamable-http"]},
                            "command": {"type": "string", "minLength": 1},
                            "url": {"type": "string", "format": "uri"},
                            "headers": {"type": "object", "additionalProperties": {"type": "string"}},
                            "args": {"type": "array", "items": {"type": "string"}},
                            "env": {"type": "object", "additionalProperties": {"type": "string"}},
                            "cwd": {"type": "string"},
                            "disabled": {"type": "boolean"},
                            "disabled_tools": {
                                "type": "array",
                                "items": {"type": "string"}
                            },
                            "startup_timeout_sec": {"type": "number", "exclusiveMinimum": 0},
                            "tool_timeout_sec": {"type": "number", "exclusiveMinimum": 0}
                        },
                        "allOf": [
                            {
                                "if": {"properties": {"transport": {"const": "stdio"}}},
                                "then": {"required": ["command"]}
                            },
                            {
                                "if": {"properties": {"transport": {"const": "streamable-http"}}},
                                "then": {"required": ["url"]}
                            }
                        ]
                    }
                },
                "connectors": {
                    "type": "array",
                    "description": "Persistent connector enablement preferences.",
                    "items": {
                        "type": "object",
                        "additionalProperties": false,
                        "required": ["name"],
                        "properties": {
                            "name": {"type": "string", "minLength": 1},
                            "disabled": {"type": "boolean"},
                            "disabled_tools": {
                                "type": "array",
                                "items": {"type": "string"}
                            }
                        }
                    }
                }
            }
        })
    }

    /// The file backing `target`, or `None` for the in-memory selection.
    fn target_path(&self, target: ConfigTarget) -> Option<PathBuf> {
        match target {
            ConfigTarget::User => Some(self.paths.user_config()),
            ConfigTarget::Project => Some(self.paths.project_config()),
            ConfigTarget::Ephemeral => None,
        }
    }
}

/// The source a write target belongs to, or `None` when the target is held in
/// memory and needs no enabled source.
const fn source_of(target: ConfigTarget) -> Option<ConfigSource> {
    match target {
        ConfigTarget::User => Some(ConfigSource::User),
        ConfigTarget::Project => Some(ConfigSource::Project),
        ConfigTarget::Ephemeral => None,
    }
}

mod integrations;
pub mod mcp;
use integrations::*;
use mcp::{decode_mcp_server, mcp_server_table, preflight_mcp_add};
use merge::merge_layer;

#[cfg(test)]
mod defaults_tests;
#[cfg(test)]
mod discovery_tests;
#[cfg(test)]
mod dotenv_tests;
#[cfg(test)]
mod events_tests;
#[cfg(test)]
mod experiments_layer_tests;
#[cfg(test)]
mod harness_tests;
#[cfg(test)]
mod introspect_tests;
#[cfg(test)]
mod mcp_parity_tests;
#[cfg(test)]
mod mcp_tests;
#[cfg(test)]
mod merge_tests;
#[cfg(test)]
mod migration_tests;
#[cfg(test)]
mod patch_tests;
#[cfg(test)]
mod persist_provider_tests;
#[cfg(test)]
mod registry_tests;
#[cfg(test)]
mod surface_parity_tests;
#[cfg(test)]
mod view_tests;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("configuration state lock is poisoned")]
    LockPoisoned,
    #[error("secure randomness is unavailable for configuration persistence")]
    RandomUnavailable,
    #[error("configuration path has no parent: `{0}`")]
    InvalidPath(PathBuf),
    #[error("configuration file exceeds the 4 MiB limit: `{0}`")]
    TooLarge(PathBuf),
    #[error("invalid TOML at `{path}`")]
    InvalidToml {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("configuration serialization failed: {0}")]
    Serialize(toml::ser::Error),
    #[error("configuration JSON serialization failed: {0}")]
    Json(serde_json::Error),
    #[error("configuration transaction journal is corrupt at `{path}`: {source}")]
    CorruptJournal {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error(
        "configuration transaction journal contains paths outside its configured targets: `{0}`"
    )]
    UnsafeJournal(PathBuf),
    #[error("configuration I/O failed at `{path}`: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("configuration target `{0:?}` appears more than once in the batch")]
    DuplicateTarget(ConfigTarget),
    #[error("project configuration is unavailable because workspace trust was revoked")]
    UntrustedProject,
    #[error("invalid provider entry: {0}")]
    InvalidProvider(String),
    /// The session did not enable the source backing this target, so writing to
    /// it would persist to a file the caller opted out of. Reference
    /// `persist_allowed`.
    #[error("configuration target `{0:?}` is not an enabled source for this session")]
    PersistenceDisabled(ConfigTarget),
    #[error("configuration changed concurrently for target `{target:?}`")]
    ConcurrentEdit { target: ConfigTarget },
    #[error(
        "configuration commit failed (`{commit}`) and rollback also failed (`{rollback}`); recovery journal retained"
    )]
    RollbackFailed { commit: String, rollback: String },
    #[error(transparent)]
    Patch(#[from] PatchError),
    /// The merged-configuration preflight refused the request, so nothing was
    /// written. Reference `ConfigPatchValidationError`.
    #[error("the configuration change was rejected: {0}")]
    PatchRejected(String),
    #[error("invalid VIBE environment key `{0}`")]
    InvalidEnvironmentKey(String),
    #[error("environment variable `{variable}` is not a valid {expected} for `{field}`")]
    InvalidEnvironmentValue {
        variable: String,
        field: String,
        expected: &'static str,
    },
    #[error("`{field}` entries require a `{merge_key}`")]
    MergeKeyMissing { field: String, merge_key: String },
    #[error("`{field}` cannot be composed by the `{strategy}` strategy across these layers")]
    MergeType {
        field: String,
        strategy: &'static str,
    },
    #[error("proxy value for `{0}` contains a forbidden control character")]
    InvalidProxyValue(ProxyKey),
    #[error("invalid MCP configuration: {0}")]
    InvalidMcp(String),
    #[error("invalid integration configuration: {0}")]
    InvalidIntegration(String),
    #[error("invalid URL in sensitive configuration field `{path}`")]
    InvalidSensitiveUrl { path: String },
    #[error("credentials are forbidden in sensitive configuration field `{path}`")]
    SensitiveUrlCredentials { path: String },
    #[error("`models` entries require an `alias` or a `name`")]
    ModelEntryWithoutAlias,
    #[error("`models` key `{key}` does not match the entry alias `{alias}`")]
    ModelAliasMismatch { key: String, alias: String },
    #[error("no model is configured; define at least one entry under `[[models]]`")]
    NoConfiguredModel,
    #[error(
        "compaction model `{alias}` names provider `{provider}`, which is not configured under `[[providers]]`"
    )]
    CompactionModelProviderMissing { alias: String, provider: String },
    #[error(
        "compaction model `{alias}` uses provider `{provider}` but the active model uses provider `{active_provider}`; they must share one"
    )]
    CompactionModelProviderMismatch {
        alias: String,
        provider: String,
        active_provider: String,
    },
    #[error("`{field}` cannot be resolved to an absolute path")]
    UnresolvablePath { field: &'static str },
}

impl From<AtomicWriteError> for ConfigError {
    fn from(error: AtomicWriteError) -> Self {
        Self::Io {
            path: error.path,
            source: error.source,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(input: &str) -> Table {
        input.parse().expect("fixture TOML")
    }

    fn config(root: &Path) -> LayeredConfig {
        let home = root.join("home/.vibe");
        let project = root.join("project");
        fs::create_dir_all(project.join(".vibe")).expect("project config directory");
        fs::create_dir_all(&home).expect("user config directory");
        LayeredConfig::new(
            ConfigPaths {
                vibe_home: home,
                working_directory: project,
            },
            table(
                r#"
active_model = "default"
thinking = "off"
[nested]
default_only = true
winner = "defaults"
"#,
            ),
        )
    }

    #[test]
    fn precedence_selects_one_toml_and_preserves_unknown_fields() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let mut config = config(temporary.path()).with_project_trusted(true);
        fs::write(
            config.paths.user_config(),
            "active_model = \"user\"\n[user_unknown]\nfuture = 1\n",
        )
        .expect("user fixture");
        fs::write(
            config.paths.project_config(),
            "active_model = \"project\"\n[future]\nunknown = \"kept\"\n",
        )
        .expect("project fixture");
        config.set_experiments(table("thinking = \"low\""));
        config.environment = BTreeMap::from([
            ("VIBE_ACTIVE_MODEL".to_owned(), "\"environment\"".to_owned()),
            (
                "VIBE_NESTED__WINNER".to_owned(),
                "\"environment\"".to_owned(),
            ),
        ]);
        config.runtime = table("active_model = \"runtime\"");
        config.agent = table("active_model = \"agent\"");

        let snapshot = config.load().expect("configuration composes");
        assert_eq!(snapshot.selected_target, ConfigTarget::Project);
        assert_eq!(snapshot.effective["active_model"].as_str(), Some("agent"));
        assert_eq!(snapshot.effective["thinking"].as_str(), Some("low"));
        assert_eq!(
            snapshot.effective["nested"]["winner"].as_str(),
            Some("environment")
        );
        assert_eq!(
            snapshot.effective["future"]["unknown"].as_str(),
            Some("kept")
        );
        assert!(snapshot.effective.get("user_unknown").is_none());
    }

    #[test]
    fn batch_write_is_conflict_checked_atomic_and_unknown_field_preserving() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let config = config(temporary.path()).with_project_trusted(true);
        fs::write(
            config.paths.user_config(),
            "known = \"old\"\n[future]\nunknown = \"kept\"\n",
        )
        .expect("user fixture");
        fs::write(config.paths.project_config(), "project = \"old\"\n").expect("project fixture");
        let before = config.load().expect("baseline loads");

        let snapshot = config
            .batch_write(&[
                ConfigWrite {
                    target: ConfigTarget::User,
                    expected_fingerprint: before.fingerprints[&ConfigTarget::User].clone(),
                    mutations: vec![ConfigMutation::set(
                        ["known"],
                        Value::String("new".to_owned()),
                    )],
                },
                ConfigWrite {
                    target: ConfigTarget::Project,
                    expected_fingerprint: before.fingerprints[&ConfigTarget::Project].clone(),
                    mutations: vec![ConfigMutation::set(
                        ["project"],
                        Value::String("new".to_owned()),
                    )],
                },
            ])
            .expect("batch commits");
        assert_eq!(snapshot.effective["project"].as_str(), Some("new"));
        let user = read_table_optional(&config.paths.user_config()).expect("user reloads");
        assert_eq!(user["known"].as_str(), Some("new"));
        assert_eq!(user["future"]["unknown"].as_str(), Some("kept"));

        fs::write(config.paths.user_config(), "known = \"external\"\n").expect("concurrent edit");
        assert!(matches!(
            config.batch_write(&[ConfigWrite {
                target: ConfigTarget::User,
                expected_fingerprint: before.fingerprints[&ConfigTarget::User].clone(),
                mutations: vec![],
            }]),
            Err(ConfigError::ConcurrentEdit {
                target: ConfigTarget::User
            })
        ));
    }

    #[test]
    fn orphan_transaction_sidecars_are_recovered_without_touching_unrelated_files() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let store = config(temporary.path());
        let user_parent = store
            .paths
            .user_config()
            .parent()
            .expect("user parent")
            .to_path_buf();
        let project_parent = store
            .paths
            .project_config()
            .parent()
            .expect("project parent")
            .to_path_buf();
        let orphan_tmp = user_parent.join(".config.0123456789abcdef0123456789abcdef.tmp");
        let orphan_bak = project_parent.join(".config.fedcba9876543210fedcba9876543210.bak");
        let unrelated = user_parent.join(".config.keep.tmp");
        fs::write(&orphan_tmp, "partial").expect("orphan temp fixture");
        fs::write(&orphan_bak, "partial").expect("orphan backup fixture");
        fs::write(&unrelated, "keep").expect("unrelated fixture");

        store.load().expect("orphan recovery succeeds");

        assert!(!orphan_tmp.exists());
        assert!(!orphan_bak.exists());
        assert!(unrelated.exists());
    }

    #[test]
    fn integration_rollbacks_use_the_committed_fingerprint_without_clobbering_a_later_writer() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let store = config(temporary.path());
        fs::write(
            store.paths.user_config(),
            r#"
[[mcp_servers]]
name = "docs"
url = "https://mcp.example.test/rpc"

[[connectors]]
name = "drive"
"#,
        )
        .expect("integration fixture");

        let mcp_commit = store
            .persist_mcp_state("docs", false, &BTreeSet::from(["search".to_owned()]))
            .expect("MCP preference commit");
        let mcp_target = mcp_commit.selected_target;
        let after_mcp_writer = store
            .batch_write(&[ConfigWrite {
                target: mcp_target,
                expected_fingerprint: mcp_commit.fingerprints[&mcp_target].clone(),
                mutations: vec![ConfigMutation::set(
                    ["writer"],
                    Value::String("after-mcp".to_owned()),
                )],
            }])
            .expect("interleaved MCP writer");
        assert!(matches!(
            store.persist_mcp_state_cas(
                "docs",
                true,
                &BTreeSet::new(),
                mcp_target,
                mcp_commit.fingerprints[&mcp_target].clone(),
            ),
            Err(ConfigError::ConcurrentEdit { target }) if target == mcp_target
        ));
        assert_eq!(
            store.load().expect("MCP conflict reloads").effective["writer"].as_str(),
            Some("after-mcp")
        );

        let connector_commit = store
            .persist_connector_state("drive", false, &BTreeSet::from(["search".to_owned()]))
            .expect("connector preference commit");
        let connector_target = connector_commit.selected_target;
        store
            .batch_write(&[ConfigWrite {
                target: connector_target,
                expected_fingerprint: connector_commit.fingerprints[&connector_target].clone(),
                mutations: vec![ConfigMutation::set(
                    ["writer"],
                    Value::String("after-connector".to_owned()),
                )],
            }])
            .expect("interleaved connector writer");
        assert!(matches!(
            store.persist_connector_state_cas(
                "drive",
                true,
                &BTreeSet::new(),
                connector_target,
                connector_commit.fingerprints[&connector_target].clone(),
            ),
            Err(ConfigError::ConcurrentEdit { target }) if target == connector_target
        ));
        let final_snapshot = store.load().expect("connector conflict reloads");
        assert_eq!(
            final_snapshot.effective["writer"].as_str(),
            Some("after-connector")
        );
        assert_ne!(
            final_snapshot.fingerprints[&connector_target],
            after_mcp_writer.fingerprints[&connector_target]
        );
    }

    #[test]
    fn runtime_and_agent_integration_overrides_win_over_selected_preferences() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let store = config(temporary.path())
            .with_runtime_overrides(table(
                r#"
[[mcp_servers]]
name = "docs"
disabled = true

[[connectors]]
name = "drive"
disabled = true
"#,
            ))
            .with_agent_overlay(table(
                r#"
[[mcp_servers]]
name = "docs"
disabled_tools = ["agent-search"]

[[connectors]]
name = "drive"
disabled_tools = ["agent-search"]
"#,
            ));
        fs::write(
            store.paths.user_config(),
            r#"
[[mcp_servers]]
name = "docs"
transport = "streamable-http"
url = "https://mcp.example.test/rpc"
disabled = false
disabled_tools = ["selected-search"]

[[connectors]]
name = "drive"
disabled = false
disabled_tools = ["selected-search"]
"#,
        )
        .expect("selected integration preferences");

        let snapshot = store.load().expect("layered integrations load");
        let mcp = snapshot
            .mcp_servers(Path::new("/workspace"))
            .expect("MCP decodes")
            .into_iter()
            .find(|server| server.alias == "docs")
            .expect("MCP remains defined");
        assert!(!mcp.enabled);
        assert_eq!(
            mcp.disabled_tools,
            BTreeSet::from(["agent-search".to_owned()])
        );
        let connector = config_array(&snapshot.effective, IntegrationCollection::Connectors)
            .expect("connectors decode")
            .into_iter()
            .find(|entry| {
                entry
                    .as_table()
                    .and_then(|entry| entry.get("name"))
                    .and_then(Value::as_str)
                    == Some("drive")
            })
            .expect("connector remains defined");
        let connector = connector.as_table().expect("connector table");
        assert_eq!(
            connector.get("disabled").and_then(Value::as_bool),
            Some(true)
        );
        assert_eq!(
            connector
                .get("disabled_tools")
                .and_then(Value::as_array)
                .and_then(|tools| tools.first())
                .and_then(Value::as_str),
            Some("agent-search")
        );
    }

    #[test]
    fn corrupt_untrusted_and_secret_bearing_inputs_fail_closed_without_leaking() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let config = config(temporary.path());
        fs::write(
            config.paths.user_config(),
            "api_key = \"parse-secret\"\nbroken = [",
        )
        .expect("corrupt configuration fixture");
        let parse_error = config.load().expect_err("corrupt TOML fails");
        assert!(matches!(parse_error, ConfigError::InvalidToml { .. }));
        assert!(!parse_error.to_string().contains("parse-secret"));

        fs::write(
            config.paths.user_config(),
            "proxy = \"https://user:secret@example.test\"\n",
        )
        .expect("proxy fixture");
        let error = config.load().expect_err("credentialed proxy rejected");
        assert!(matches!(error, ConfigError::SensitiveUrlCredentials { .. }));
        assert!(!error.to_string().contains("secret"));

        let baseline =
            fingerprint_optional(&config.paths.project_config()).expect("fingerprint reads");
        assert!(matches!(
            config.batch_write(&[ConfigWrite {
                target: ConfigTarget::Project,
                expected_fingerprint: baseline,
                mutations: vec![],
            }]),
            Err(ConfigError::UntrustedProject)
        ));
    }

    #[test]
    fn public_views_redact_secrets_and_trust_revocation_switches_persistence() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let config = config(temporary.path()).with_project_trusted(true);
        fs::write(
            config.paths.project_config(),
            "api_key = \"top-secret\"\nprivateKey = \"private\"\nproxy = \"https://example.test/?access_token=query-secret\"\nactive_model = \"project\"\n",
        )
        .expect("project fixture");
        let trusted = config.load().expect("trusted project loads");
        assert_eq!(trusted.selected_target, ConfigTarget::Project);
        assert_eq!(trusted.public_view()["config"]["api_key"], "[redacted]");
        assert_eq!(trusted.public_view()["config"]["privateKey"], "[redacted]");
        assert_eq!(trusted.public_view()["config"]["proxy"], "[redacted]");
        assert_eq!(
            trusted.public_view()["targetValues"]["project"]["api_key"],
            "[redacted]"
        );
        assert!(
            trusted.public_view()["layerValues"]
                .as_array()
                .is_some_and(|layers| layers.iter().all(|layer| {
                    layer
                        .pointer("/values/api_key")
                        .is_none_or(|value| value == "[redacted]")
                }))
        );

        let revoked = config
            .with_project_trusted(false)
            .load()
            .expect("user fallback");
        assert_eq!(revoked.selected_target, ConfigTarget::User);
        assert_ne!(revoked.effective["active_model"].as_str(), Some("project"));
        assert_eq!(revoked.public_view()["targetValues"]["project"], json!({}));
    }

    #[test]
    fn typed_mcp_stdio_entries_preserve_argv_limits_filters_and_working_directory() {
        let working_directory = PathBuf::from("/workspace");
        let snapshot = ConfigSnapshot {
            effective: table(
                r#"
[[mcp_servers]]
name = "fixture"
transport = "stdio"
command = "/usr/bin/fixture"
args = ["--stdio"]
env = { TOKEN = "secret" }
cwd = "tools"
startup_timeout_sec = 1.5
tool_timeout_sec = 2
disabled_tools = ["admin"]
"#,
            ),
            selected_target: ConfigTarget::User,
            selected_path: PathBuf::from("/home/user/.vibe/config.toml"),
            fingerprints: BTreeMap::new(),
            target_values: BTreeMap::new(),
            layer_values: Vec::new(),
            validation_warnings: Vec::new(),
        };
        let servers = snapshot
            .mcp_servers(&working_directory)
            .expect("typed MCP configuration");
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].alias, "fixture");
        assert_eq!(servers[0].startup_timeout_ms, 1_500);
        assert_eq!(servers[0].tool_timeout_ms, 2_000);
        assert_eq!(
            servers[0].disabled_tools,
            BTreeSet::from(["admin".to_owned()])
        );
        assert!(matches!(
            &servers[0].transport,
            McpTransportConfig::Stdio {
                command,
                arguments,
                environment,
                working_directory: Some(cwd),
            } if command == "/usr/bin/fixture"
                && arguments == &["--stdio"]
                && environment.get("TOKEN").is_some_and(|value| value == "secret")
                && cwd == &working_directory.join("tools")
        ));
    }

    #[test]
    fn integration_mutations_persist_and_mcp_collisions_fail_before_overwrite() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let store = config(temporary.path());
        let server = McpServerConfig {
            alias: "docs".to_owned(),
            transport: McpTransportConfig::StreamableHttp {
                url: Url::parse("https://mcp.example.test/rpc").expect("fixture URL"),
                headers: BTreeMap::new(),
            },
            enabled: true,
            disabled_tools: BTreeSet::new(),
            startup_timeout_ms: 1_500,
            tool_timeout_ms: 2_000,
            auth: Default::default(),
            prompt: None,
            sampling_enabled: true,
        };

        store.persist_mcp_add(&server).expect("MCP persists");
        store
            .persist_mcp_state("docs", false, &BTreeSet::from(["search".to_owned()]))
            .expect("MCP state persists");
        let configured = store
            .load()
            .expect("configuration reloads")
            .mcp_servers(Path::new("/workspace"))
            .expect("MCP configuration decodes");
        assert_eq!(configured.len(), 1);
        assert!(!configured[0].enabled);
        assert_eq!(
            configured[0].disabled_tools,
            BTreeSet::from(["search".to_owned()])
        );

        assert!(store.preflight_mcp_add(&server).is_err());
        let same_url = McpServerConfig {
            alias: "other".to_owned(),
            ..server.clone()
        };
        assert!(store.preflight_mcp_add(&same_url).is_err());

        store
            .persist_connector_state(
                "github",
                false,
                &BTreeSet::from(["create_issue".to_owned()]),
            )
            .expect("connector state persists");
        let preferences = store
            .connector_preferences()
            .expect("connector preferences reload");
        assert_eq!(
            preferences.get("github"),
            Some(&IntegrationPreference {
                enabled: false,
                disabled_tools: BTreeSet::from(["create_issue".to_owned()]),
            })
        );
    }

    #[test]
    fn integration_mutations_never_copy_higher_layer_secrets_into_the_selected_file() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let store = config(temporary.path()).with_experiments(table(
            r#"
[[mcp_servers]]
name = "inherited"
transport = "stdio"
command = "/usr/bin/inherited"
env = { API_TOKEN = "must-not-be-copied" }
"#,
        ));
        let server = McpServerConfig {
            alias: "selected".to_owned(),
            transport: McpTransportConfig::StreamableHttp {
                url: Url::parse("https://selected.example.test/mcp").expect("fixture URL"),
                headers: BTreeMap::new(),
            },
            enabled: true,
            disabled_tools: BTreeSet::new(),
            startup_timeout_ms: 1_000,
            tool_timeout_ms: 1_000,
            auth: Default::default(),
            prompt: None,
            sampling_enabled: true,
        };

        store
            .persist_mcp_add(&server)
            .expect("selected MCP persists");
        let aliases = store
            .load()
            .expect("merged integrations reload")
            .mcp_servers(Path::new("/workspace"))
            .expect("merged MCP configuration")
            .into_iter()
            .map(|config| config.alias)
            .collect::<BTreeSet<_>>();
        assert_eq!(
            aliases,
            BTreeSet::from(["inherited".to_owned(), "selected".to_owned()])
        );

        store
            .persist_mcp_state("inherited", false, &BTreeSet::from(["search".to_owned()]))
            .expect("inherited MCP preference persists");
        let inherited = store
            .load()
            .expect("preference reloads")
            .mcp_servers(Path::new("/workspace"))
            .expect("effective MCP configuration")
            .into_iter()
            .find(|config| config.alias == "inherited")
            .expect("inherited MCP remains visible");
        assert!(!inherited.enabled);
        assert_eq!(
            inherited.disabled_tools,
            BTreeSet::from(["search".to_owned()])
        );

        let selected = fs::read_to_string(store.paths.user_config()).expect("selected file reads");
        assert!(selected.contains("selected"));
        assert!(selected.contains("inherited"));
        assert!(!selected.contains("/usr/bin/inherited"));
        assert!(!selected.contains("must-not-be-copied"));
    }

    #[test]
    fn duplicate_mcp_entries_fail_without_echoing_secret_bearing_values() {
        let snapshot = ConfigSnapshot {
            effective: table(
                r#"
[[mcp_servers]]
name = "duplicate"
transport = "stdio"
command = "/first"

[[mcp_servers]]
name = "duplicate"
transport = "stdio"
command = "top-secret-command"
"#,
            ),
            selected_target: ConfigTarget::User,
            selected_path: PathBuf::from("/home/user/.vibe/config.toml"),
            fingerprints: BTreeMap::new(),
            target_values: BTreeMap::new(),
            layer_values: Vec::new(),
            validation_warnings: Vec::new(),
        };
        let error = snapshot
            .mcp_servers(Path::new("/workspace"))
            .expect_err("duplicate aliases fail closed")
            .to_string();
        assert!(error.contains("unique"));
        assert!(!error.contains("top-secret-command"));
    }

    #[test]
    fn unsupported_mcp_transport_fails_closed() {
        let snapshot = ConfigSnapshot {
            effective: table(
                r#"
[[mcp_servers]]
name = "invalid"
transport = "python"
command = "must-not-run"
"#,
            ),
            selected_target: ConfigTarget::User,
            selected_path: PathBuf::from("/home/user/.vibe/config.toml"),
            fingerprints: BTreeMap::new(),
            target_values: BTreeMap::new(),
            layer_values: Vec::new(),
            validation_warnings: Vec::new(),
        };
        let error = snapshot
            .mcp_servers(Path::new("/workspace"))
            .expect_err("unsupported transport fails closed")
            .to_string();
        assert!(error.contains("`http`, `streamable-http` or `stdio`"));
        assert!(!error.contains("must-not-run"));
    }

    #[test]
    fn recovery_rejects_journal_paths_outside_configured_targets() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let config = config(temporary.path());
        let victim = temporary.path().join("victim.txt");
        let backup = temporary.path().join(".config.1.bak");
        let temporary_file = temporary.path().join(".config.1.tmp");
        fs::write(&victim, "keep").expect("victim fixture");
        fs::write(&backup, "replace").expect("backup fixture");
        fs::write(&temporary_file, "temporary").expect("temporary fixture");
        let journal = ConfigJournal {
            state: JournalState::Prepared,
            entries: vec![transaction::JournalEntry {
                destination: victim.clone(),
                temporary: temporary_file,
                backup,
                had_original: true,
            }],
        };
        fs::write(
            config.paths.vibe_home.join(TRANSACTION_FILE),
            serde_json::to_vec(&journal).expect("journal serializes"),
        )
        .expect("journal fixture");

        assert!(matches!(config.load(), Err(ConfigError::UnsafeJournal(_))));
        assert_eq!(fs::read_to_string(victim).expect("victim remains"), "keep");
    }
}
