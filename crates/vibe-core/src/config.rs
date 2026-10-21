use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value as JsonValue};
use toml::Table;

use crate::text::sha256_hex;
use document::{fingerprint_optional, read_table_optional, validate_table};
use effective::finalize_effective;
use environment::environment_table;
use merge::merge_layer;
use transaction::{
    ConfigFileLock, cleanup_orphan_sidecars, ensure_private_directory, recover_transaction,
};

pub mod admin;
mod document;
pub mod dotenv;
mod effective;
mod encode;
mod environment;
mod error;
pub mod events;
pub mod experiments_layer;
pub mod harness;
mod integrations;
pub mod introspect;
pub mod mcp;
mod merge;
pub mod migration;
mod models;
pub mod patch;
mod providers;
mod proxy;
pub mod registry;
mod snapshot;
mod transaction;
mod view;
mod write;

#[cfg(test)]
mod config_tests;
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
mod layers_parity_tests;
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

pub use dotenv::{DotenvValues, global_env_file, inherited_by_children};
pub use encode::encode_document;
pub use error::ConfigError;
pub use events::{ConfigChangeBus, ConfigChangeEvent, ConfigSubscription};
pub use experiments_layer::{
    EXPERIMENTS_LAYER_NAME, ExperimentsLayer, MAPPED_EXPERIMENTS, PromptResolves, configured_fields,
};
pub use harness::{ConfigSource, HarnessFiles};
pub use introspect::{ConfigFieldView, ConfigFields, ConfigLayerValue, HIDDEN_FIELDS};
pub use models::{active_model_alias, available_model_aliases, default_model_alias};
pub use patch::{ConfigMutation, JsonPointer, PatchError, PatchOperation};
pub use proxy::{ProxyEnvironmentStore, ProxyKey, ProxyKeyError};
pub use snapshot::{ConfigSnapshot, IntegrationPreference};
pub use write::{ConfigPatchOp, ConfigPatchOutcome, ConfigWrite};

/// The alias new turns run on, empty where the operator pinned nothing.
const ACTIVE_MODEL_FIELD: &str = "active_model";
/// The alias an experiment routes an unpinned installation onto.
const ROUTED_DEFAULT_MODEL_FIELD: &str = "routed_default_model";
/// The definition of that alias, carried by the same experiment.
const ROUTED_MODEL_CONFIG_FIELD: &str = "routed_model_config";

/// The fields an agent profile may never override, sorted. Reference
/// `PROTECTED_FIELDS` in `vibe/core/config/layers/agent_profile.py`.
pub const AGENT_PROTECTED_FIELDS: [&str; 3] = [
    "console_base_url",
    "vibe_base_url",
    "vibe_code_sessions_base_url",
];

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
    /// The user file, `{vibe_home}/config.toml`. Reference `UserConfigLayer`.
    UserToml,
    /// The discovered project file, composed only while it is trusted and
    /// above the user file, so a project inherits every key it leaves unset.
    /// Reference `ProjectConfigLayer`.
    ProjectToml,
    Environment,
    Runtime,
    Agent,
    /// What an organization enforces, fetched at session start. Reference
    /// `AdminConfigLayer`, the highest-priority layer: what it sets shadows
    /// every other layer, which is the whole enforcement mechanism. Held in
    /// memory only and fetched again by the next process.
    Admin,
}

impl ConfigLayerKind {
    /// The name the reference gives the layer, which is what `config/fields/read`
    /// reports a value's origin by: `ConfigLayer.name` of `DefaultConfigLayer`,
    /// `DiscoveredConfigLayer`, `GrowthbookLayer`, `UserConfigLayer`,
    /// `ProjectConfigLayer`, `EnvironmentLayer`, `OverridesLayer`,
    /// `AgentProfileLayer` and `AdminConfigLayer` (`vibe/core/config/layers/`).
    #[must_use]
    pub const fn reference_name(self) -> &'static str {
        match self {
            Self::Defaults => "default",
            Self::Discovered => "discovered",
            Self::Experiments => "growthbook",
            Self::UserToml => "user-toml",
            Self::ProjectToml => "project-toml",
            Self::Environment => "environment",
            Self::Runtime => "overrides",
            Self::Agent => "agent-profile",
            Self::Admin => "admin",
        }
    }
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
    /// The document behind [`ConfigLayerKind::Admin`], shared by every clone
    /// for the same reason the experiment layer is.
    admin: Arc<Mutex<Table>>,
    /// Whether the rollout that may route this store's sessions onto another
    /// model is still being looked up, shared for the same reason.
    experiment_model_pending: Arc<AtomicBool>,
    runtime: Table,
    agent: Table,
    environment: BTreeMap<String, String>,
    project_trusted: bool,
    /// See [`HarnessFiles::with_project_file_trust`].
    project_file_trust: Option<bool>,
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
    /// The directory a relative path in this configuration resolves against.
    #[must_use]
    pub fn working_directory(&self) -> &Path {
        &self.paths.working_directory
    }

    #[must_use]
    pub fn new(paths: ConfigPaths, defaults: Table) -> Self {
        Self {
            paths,
            defaults,
            discovery: None,
            experiments: Arc::new(Mutex::new(Table::new())),
            admin: Arc::new(Mutex::new(Table::new())),
            experiment_model_pending: Arc::new(AtomicBool::new(false)),
            runtime: Table::new(),
            agent: Table::new(),
            environment: BTreeMap::new(),
            project_trusted: false,
            project_file_trust: None,
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
        .with_project_file_trust(self.project_file_trust)
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

    /// Publishes whether a rollout that may route this store's sessions onto
    /// another model is still being looked up, which every later snapshot
    /// reports as [`ConfigSnapshot::awaiting_experiment_model`].
    pub fn set_experiment_model_pending(&self, pending: bool) {
        self.experiment_model_pending
            .store(pending, Ordering::Release);
    }

    /// Publishes the variants a resolved rollout assigned, mapped onto the
    /// fields the layer writes.
    ///
    /// Reference `_sync_growthbook_layer_variants`.
    pub fn set_experiment_variants(
        &self,
        variants: &Map<String, JsonValue>,
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
        variants: &Map<String, JsonValue>,
        prompt_resolves: PromptResolves,
    ) -> Self {
        self.set_experiment_variants(variants, prompt_resolves);
        self
    }

    /// Replaces what [`ConfigLayerKind::Admin`] composes, for every handle
    /// onto this store, and answers what it held before, so a document that
    /// fails validation can be put back. Reference
    /// `AdminConfigLayer.load_managed_toml` and `restore`.
    pub fn set_admin(&self, values: Table) -> Table {
        std::mem::replace(
            &mut *self
                .admin
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            values,
        )
    }

    #[must_use]
    pub fn with_runtime_overrides(mut self, values: Table) -> Self {
        self.runtime = values;
        self
    }

    /// Installs the active agent profile's overrides, without the fields that
    /// route credentials.
    ///
    /// Reference `AgentProfileLayer._strip_protected`: the account call sends
    /// the API key to `console_base_url` and Teleport sends it to
    /// `vibe_code_sessions_base_url`, so a profile shipped by an untrusted
    /// checkout could otherwise redirect the key to a host of its choosing.
    #[must_use]
    ///
    /// The reference also writes a log line naming what it dropped; this crate
    /// keeps no log, so the fields are dropped without one.
    pub fn with_agent_overlay(mut self, mut values: Table) -> Self {
        for field in AGENT_PROTECTED_FIELDS {
            values.remove(field);
        }
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
        scoped.project_file_trust = None;
        scoped
    }

    /// Pins whether the project configuration file may be read, as a session
    /// resolved it when it started, instead of asking the trust store on every
    /// load. Reference `BaseConfigLayer` caches the verdict on the layer, so a
    /// trust decision made mid-session reaches the project roots but not the
    /// project file.
    #[must_use]
    pub fn with_project_file_trust(mut self, trust: Option<bool>) -> Self {
        self.project_file_trust = trust;
        self
    }

    /// Whether a write may land in the project file: the verdict a session
    /// pinned for it when there is one, the workspace's trust otherwise. The
    /// reference refuses a patch to a project layer it did not load, so a
    /// file this configuration does not read is not written either.
    fn project_writable(&self) -> bool {
        self.project_file_trust.unwrap_or(self.project_trusted)
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
        let user_enabled = self.sources.contains(&ConfigSource::User);
        let trusted_project = harness.trusted_project_config();
        // The selection is the file an implicit write lands in. Reference
        // `build_default_orchestrator`'s `default_layer_resolver`: the user
        // file while that source is enabled, since a project file found by
        // walking up is rarely the scope a write meant, then a trusted project
        // file, then the in-memory document.
        let (selected_target, selected_path) = harness
            .config_file()
            .unwrap_or((ConfigTarget::Ephemeral, PathBuf::new()));

        let user_values = if user_enabled {
            read_table_optional(&user_path)?
        } else {
            Table::new()
        };
        let project_values = match &trusted_project {
            Some(trusted) => read_table_optional(trusted)?,
            None => Table::new(),
        };
        let mut target_values = BTreeMap::from([
            (ConfigTarget::User, user_values),
            (ConfigTarget::Project, project_values),
        ]);
        if selected_target == ConfigTarget::Ephemeral {
            target_values.insert(ConfigTarget::Ephemeral, self.ephemeral_document()?);
        }
        // The in-memory document is written where reference writes reach its
        // overrides layer, so it composes with the runtime overrides.
        let mut runtime = self.runtime.clone();
        if let Some(ephemeral) = target_values.get(&ConfigTarget::Ephemeral) {
            merge_layer(&mut runtime, ephemeral)?;
        }
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
                kind: ConfigLayerKind::UserToml,
                values: target_values
                    .get(&ConfigTarget::User)
                    .cloned()
                    .unwrap_or_default(),
            },
            ConfigLayer {
                kind: ConfigLayerKind::ProjectToml,
                values: target_values
                    .get(&ConfigTarget::Project)
                    .cloned()
                    .unwrap_or_default(),
            },
            ConfigLayer {
                kind: ConfigLayerKind::Environment,
                values: environment,
            },
            ConfigLayer {
                kind: ConfigLayerKind::Runtime,
                values: runtime,
            },
            ConfigLayer {
                kind: ConfigLayerKind::Agent,
                values: self.agent.clone(),
            },
            ConfigLayer {
                kind: ConfigLayerKind::Admin,
                values: self
                    .admin
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone(),
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
        // Reference `validate_merged` reads the origin of the allowlist and of
        // the global threshold: the admin layer is the highest, so it is the
        // origin of a key exactly when it sets it.
        let admin = layers
            .iter()
            .find(|layer| layer.kind == ConfigLayerKind::Admin)
            .map(|layer| layer.values.clone())
            .unwrap_or_default();
        validation_warnings.extend(finalize_effective(
            &mut effective,
            &self.paths.vibe_home,
            &admin,
        )?);
        let prompts_dirs = self.harness_files().prompts_dirs();
        if let Some(prompt_id) = effective
            .get(crate::system_prompt::SYSTEM_PROMPT_SETTING)
            .and_then(toml::Value::as_str)
        {
            crate::system_prompt::load_system_prompt(prompt_id, &prompts_dirs)
                .map_err(ConfigError::SystemPrompt)?;
        }
        // Reference `_check_compaction_prompt`: an identifier that names no
        // prompt refuses the configuration rather than the first compaction.
        if let Some(prompt_id) = effective
            .get(crate::compaction::manager::CompactionPromptResolution::SETTING)
            .and_then(toml::Value::as_str)
        {
            crate::prompt::library::load_prompt(
                prompt_id,
                crate::compaction::manager::CompactionPromptResolution::SETTING,
                &prompts_dirs,
                &[crate::prompt::library::UtilityPrompt::Compact],
            )
            .map_err(|error| ConfigError::CompactionPrompt(Box::new(error)))?;
        }

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
                Some(sha256_hex(
                    target_values
                        .get(&ConfigTarget::Ephemeral)
                        .map(Table::to_string)
                        .unwrap_or_default()
                        .as_bytes(),
                )),
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
            awaiting_experiment_model: self.experiment_model_pending.load(Ordering::Acquire),
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

    /// The in-memory document behind [`ConfigTarget::Ephemeral`].
    fn ephemeral_document(&self) -> Result<Table, ConfigError> {
        Ok(self
            .ephemeral
            .lock()
            .map_err(|_| ConfigError::LockPoisoned)?
            .clone())
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
