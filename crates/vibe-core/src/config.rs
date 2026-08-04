use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::{Value as JsonValue, json};
use sha2::{Digest, Sha256};
use thiserror::Error;
use toml::{Table, Value};
use url::Url;

use crate::atomic_file::{self, AtomicWriteError, create_private_file, write_atomically};
use crate::mcp::{
    DEFAULT_MCP_STARTUP_TIMEOUT_MS, DEFAULT_MCP_TOOL_TIMEOUT_MS, McpServerConfig,
    McpTransportConfig,
};
use crate::text::hex_encode;

mod merge;
mod proxy;
pub mod registry;

pub use proxy::{ProxyEnvironmentStore, ProxyKey, ProxyKeyError};

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
    SelectedToml,
    Experiments,
    Environment,
    Runtime,
    Agent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigTarget {
    User,
    Project,
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

    #[must_use]
    pub fn project_config(&self) -> PathBuf {
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
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IntegrationPreference {
    pub enabled: bool,
    pub disabled_tools: BTreeSet<String>,
}

impl ConfigSnapshot {
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
            let transport = required_mcp_string(table, "transport")?;
            let alias = required_mcp_string(table, "name")?.to_owned();
            if !aliases.insert(alias.clone()) {
                return Err(ConfigError::InvalidMcp(
                    "MCP server names must be unique".to_owned(),
                ));
            }
            let transport = match transport {
                "stdio" => {
                    let command = required_mcp_string(table, "command")?.to_owned();
                    let arguments = optional_mcp_strings(table, "args")?;
                    let environment = optional_mcp_environment_at(table, "env")?;
                    let working_directory = optional_mcp_string(table, "cwd")?
                        .map(PathBuf::from)
                        .map(|path| {
                            if path.is_absolute() {
                                path
                            } else {
                                working_directory.join(path)
                            }
                        })
                        .or_else(|| Some(working_directory.to_path_buf()));
                    McpTransportConfig::Stdio {
                        command,
                        arguments,
                        environment,
                        working_directory,
                    }
                }
                "streamable-http" => {
                    let url = Url::parse(required_mcp_string(table, "url")?).map_err(|_| {
                        ConfigError::InvalidMcp(
                            "MCP server field `url` must be a valid URL".to_owned(),
                        )
                    })?;
                    let headers = optional_mcp_environment_at(table, "headers")?;
                    McpTransportConfig::StreamableHttp { url, headers }
                }
                _ => {
                    return Err(ConfigError::InvalidMcp(
                        "MCP transport must be stdio or streamable-http".to_owned(),
                    ));
                }
            };
            let disabled = optional_mcp_bool(table, "disabled")?.unwrap_or(false);
            let disabled_tools = optional_mcp_strings(table, "disabled_tools")?
                .into_iter()
                .collect();
            servers.push(McpServerConfig {
                alias,
                transport,
                enabled: !disabled,
                disabled_tools,
                startup_timeout_ms: optional_mcp_timeout(
                    table,
                    "startup_timeout_sec",
                    DEFAULT_MCP_STARTUP_TIMEOUT_MS,
                )?,
                tool_timeout_ms: optional_mcp_timeout(
                    table,
                    "tool_timeout_sec",
                    DEFAULT_MCP_TOOL_TIMEOUT_MS,
                )?,
            });
        }
        Ok(servers)
    }

    pub fn mcp_aliases(&self) -> Result<BTreeSet<String>, ConfigError> {
        config_array(&self.effective, IntegrationCollection::McpServers)?
            .into_iter()
            .map(|entry| {
                entry
                    .as_table()
                    .and_then(|entry| entry.get("name"))
                    .and_then(Value::as_str)
                    .filter(|name| !name.is_empty())
                    .map(str::to_owned)
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
pub struct ConfigMutation {
    pub path: Vec<String>,
    pub value: Option<Value>,
}

impl ConfigMutation {
    #[must_use]
    pub fn set(path: impl IntoIterator<Item = impl Into<String>>, value: Value) -> Self {
        Self {
            path: path.into_iter().map(Into::into).collect(),
            value: Some(value),
        }
    }

    #[must_use]
    pub fn remove(path: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            path: path.into_iter().map(Into::into).collect(),
            value: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ConfigWrite {
    pub target: ConfigTarget,
    pub expected_fingerprint: Option<String>,
    pub mutations: Vec<ConfigMutation>,
}

#[derive(Clone)]
pub struct LayeredConfig {
    paths: ConfigPaths,
    defaults: Table,
    experiments: Table,
    runtime: Table,
    agent: Table,
    environment: BTreeMap<String, String>,
    project_trusted: bool,
    transaction_lock: Arc<Mutex<()>>,
}

impl LayeredConfig {
    #[must_use]
    pub fn new(paths: ConfigPaths, defaults: Table) -> Self {
        Self {
            paths,
            defaults,
            experiments: Table::new(),
            runtime: Table::new(),
            agent: Table::new(),
            environment: BTreeMap::new(),
            project_trusted: false,
            transaction_lock: Arc::new(Mutex::new(())),
        }
    }

    #[must_use]
    pub fn with_experiments(mut self, values: Table) -> Self {
        self.experiments = values;
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

    pub fn load(&self) -> Result<ConfigSnapshot, ConfigError> {
        let _guard = self
            .transaction_lock
            .lock()
            .map_err(|_| ConfigError::LockPoisoned)?;
        ensure_private_directory(&self.paths.vibe_home)?;
        let _file_guard = ConfigFileLock::acquire(&self.paths.vibe_home)?;
        recover_transaction(&self.paths)?;
        cleanup_orphan_sidecars(&self.paths)?;

        let user_path = self.paths.user_config();
        let project_path = self.paths.project_config();
        let selected_project = self
            .project_trusted
            .then_some(project_path.clone())
            .filter(|path| path.is_file());
        let (selected_target, selected_path) = selected_project.map_or_else(
            || (ConfigTarget::User, user_path.clone()),
            |path| (ConfigTarget::Project, path),
        );

        let user_values = read_table_optional(&user_path)?;
        let project_values = if self.project_trusted {
            read_table_optional(&project_path)?
        } else {
            Table::new()
        };
        let target_values = BTreeMap::from([
            (ConfigTarget::User, user_values),
            (ConfigTarget::Project, project_values),
        ]);
        let selected = target_values
            .get(&selected_target)
            .cloned()
            .unwrap_or_default();
        let environment = environment_table(&self.environment)?;
        let layers = vec![
            ConfigLayer {
                kind: ConfigLayerKind::Defaults,
                values: self.defaults.clone(),
            },
            ConfigLayer {
                kind: ConfigLayerKind::SelectedToml,
                values: selected.clone(),
            },
            ConfigLayer {
                kind: ConfigLayerKind::Experiments,
                values: self.experiments.clone(),
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

        let fingerprints = BTreeMap::from([
            (
                ConfigTarget::User,
                fingerprint_optional(&self.paths.user_config())?,
            ),
            (ConfigTarget::Project, fingerprint_optional(&project_path)?),
        ]);
        Ok(ConfigSnapshot {
            effective,
            selected_target,
            selected_path,
            fingerprints,
            target_values,
            layer_values: layers,
        })
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
        for write in writes {
            if !targets.insert(write.target) {
                return Err(ConfigError::DuplicateTarget(write.target));
            }
            if write.target == ConfigTarget::Project && !self.project_trusted {
                return Err(ConfigError::UntrustedProject);
            }
            let path = self.target_path(write.target);
            let actual_fingerprint = fingerprint_optional(&path)?;
            if actual_fingerprint != write.expected_fingerprint {
                return Err(ConfigError::ConcurrentEdit {
                    target: write.target,
                });
            }
            let mut table = read_table_optional(&path)?;
            for mutation in &write.mutations {
                apply_mutation(&mut table, mutation)?;
            }
            validate_table(&table)?;
            let encoded = toml::to_string_pretty(&table).map_err(ConfigError::Serialize)?;
            prepared.push(PreparedWrite::new(path, encoded.into_bytes())?);
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
        drop(_file_guard);
        drop(_guard);
        self.load()
    }

    pub fn preflight_mcp_add(&self, config: &McpServerConfig) -> Result<(), ConfigError> {
        let snapshot = self.load()?;
        preflight_mcp_add(&snapshot.effective, config)
    }

    pub fn persist_mcp_add(&self, config: &McpServerConfig) -> Result<ConfigSnapshot, ConfigError> {
        let snapshot = self.load()?;
        preflight_mcp_add(&snapshot.effective, config)?;
        let target = snapshot.selected_target;
        let mut entries =
            config_array_for_target(&snapshot, target, IntegrationCollection::McpServers)?;
        entries.push(mcp_server_value(config)?);
        self.replace_array_cas(
            target,
            snapshot.fingerprints.get(&target).cloned().flatten(),
            IntegrationCollection::McpServers.key(),
            entries,
        )
    }

    pub fn persist_mcp_state(
        &self,
        alias: &str,
        enabled: bool,
        disabled_tools: &BTreeSet<String>,
    ) -> Result<ConfigSnapshot, ConfigError> {
        self.persist_integration_state(
            IntegrationCollection::McpServers,
            alias,
            enabled,
            disabled_tools,
        )
    }

    pub fn persist_mcp_state_cas(
        &self,
        alias: &str,
        enabled: bool,
        disabled_tools: &BTreeSet<String>,
        target: ConfigTarget,
        expected_fingerprint: Option<String>,
    ) -> Result<ConfigSnapshot, ConfigError> {
        self.persist_integration_state_cas(
            IntegrationCollection::McpServers,
            alias,
            enabled,
            disabled_tools,
            target,
            expected_fingerprint,
        )
    }

    /// Persists enablement against the target the snapshot selected.
    fn persist_integration_state(
        &self,
        collection: IntegrationCollection,
        name: &str,
        enabled: bool,
        disabled_tools: &BTreeSet<String>,
    ) -> Result<ConfigSnapshot, ConfigError> {
        let snapshot = self.load()?;
        let target = snapshot.selected_target;
        let expected_fingerprint = snapshot.fingerprints.get(&target).cloned().flatten();
        self.persist_integration_state_cas(
            collection,
            name,
            enabled,
            disabled_tools,
            target,
            expected_fingerprint,
        )
    }

    /// Persists enablement for one entry, failing if the target changed.
    ///
    /// The entry is created in the target when it is only present in another
    /// layer, so disabling a default-provided server writes an explicit record.
    fn persist_integration_state_cas(
        &self,
        collection: IntegrationCollection,
        name: &str,
        enabled: bool,
        disabled_tools: &BTreeSet<String>,
        target: ConfigTarget,
        expected_fingerprint: Option<String>,
    ) -> Result<ConfigSnapshot, ConfigError> {
        let snapshot = self.load()?;
        if collection == IntegrationCollection::McpServers
            && !config_array(&snapshot.effective, collection)?
                .iter()
                .filter_map(Value::as_table)
                .any(|entry| collection.identity(entry) == Some(name))
        {
            return Err(collection.invalid(&format!("unknown MCP server `{name}`")));
        }
        let mut entries = config_array_for_target(&snapshot, target, collection)?;
        let position = entries
            .iter()
            .position(|entry| {
                entry
                    .as_table()
                    .and_then(|entry| collection.identity(entry))
                    == Some(name)
            })
            .unwrap_or_else(|| {
                let mut entry = Table::new();
                entry.insert("name".to_owned(), Value::String(name.to_owned()));
                entries.push(Value::Table(entry));
                entries.len().saturating_sub(1)
            });
        let entry = entries
            .get_mut(position)
            .and_then(Value::as_table_mut)
            .ok_or_else(|| {
                collection.invalid(&format!("{} entry must be a table", collection.key()))
            })?;
        entry.insert("disabled".to_owned(), Value::Boolean(!enabled));
        entry.insert(
            "disabled_tools".to_owned(),
            Value::Array(disabled_tools.iter().cloned().map(Value::String).collect()),
        );
        self.replace_array_cas(target, expected_fingerprint, collection.key(), entries)
    }

    pub fn connector_preferences(
        &self,
    ) -> Result<BTreeMap<String, IntegrationPreference>, ConfigError> {
        let snapshot = self.load()?;
        let entries = config_array(&snapshot.effective, IntegrationCollection::Connectors)?;
        let mut preferences = BTreeMap::new();
        for entry in entries {
            let entry = entry.as_table().ok_or_else(|| {
                ConfigError::InvalidIntegration("each connectors entry must be a table".to_owned())
            })?;
            let name = entry
                .get("name")
                .or_else(|| entry.get("id"))
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty())
                .ok_or_else(|| {
                    ConfigError::InvalidIntegration(
                        "connector field `name` must be a non-empty string".to_owned(),
                    )
                })?;
            let enabled = match entry.get("disabled") {
                None => true,
                Some(Value::Boolean(disabled)) => !disabled,
                Some(_) => {
                    return Err(ConfigError::InvalidIntegration(
                        "connector field `disabled` must be a boolean".to_owned(),
                    ));
                }
            };
            let disabled_tools = match entry.get("disabled_tools") {
                None => BTreeSet::new(),
                Some(Value::Array(values)) => values
                    .iter()
                    .map(|value| {
                        value.as_str().map(str::to_owned).ok_or_else(|| {
                            ConfigError::InvalidIntegration(
                                "connector field `disabled_tools` must contain strings".to_owned(),
                            )
                        })
                    })
                    .collect::<Result<_, _>>()?,
                Some(_) => {
                    return Err(ConfigError::InvalidIntegration(
                        "connector field `disabled_tools` must be an array".to_owned(),
                    ));
                }
            };
            if preferences
                .insert(
                    name.to_owned(),
                    IntegrationPreference {
                        enabled,
                        disabled_tools,
                    },
                )
                .is_some()
            {
                return Err(ConfigError::InvalidIntegration(format!(
                    "connector `{name}` appears more than once"
                )));
            }
        }
        Ok(preferences)
    }

    pub fn persist_connector_state(
        &self,
        name: &str,
        enabled: bool,
        disabled_tools: &BTreeSet<String>,
    ) -> Result<ConfigSnapshot, ConfigError> {
        self.persist_integration_state(
            IntegrationCollection::Connectors,
            name,
            enabled,
            disabled_tools,
        )
    }

    pub fn persist_connector_state_cas(
        &self,
        name: &str,
        enabled: bool,
        disabled_tools: &BTreeSet<String>,
        target: ConfigTarget,
        expected_fingerprint: Option<String>,
    ) -> Result<ConfigSnapshot, ConfigError> {
        self.persist_integration_state_cas(
            IntegrationCollection::Connectors,
            name,
            enabled,
            disabled_tools,
            target,
            expected_fingerprint,
        )
    }

    fn replace_array_cas(
        &self,
        target: ConfigTarget,
        expected_fingerprint: Option<String>,
        key: &str,
        entries: Vec<Value>,
    ) -> Result<ConfigSnapshot, ConfigError> {
        self.batch_write(&[ConfigWrite {
            target,
            expected_fingerprint,
            mutations: vec![ConfigMutation::set([key], Value::Array(entries))],
        }])
    }

    /// The JSON Schema for the published configuration surface, generated from
    /// [`registry::FIELDS`].
    #[must_use]
    pub fn schema() -> JsonValue {
        registry::json_schema()
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

    fn target_path(&self, target: ConfigTarget) -> PathBuf {
        match target {
            ConfigTarget::User => self.paths.user_config(),
            ConfigTarget::Project => self.paths.project_config(),
        }
    }
}

mod integrations;
use integrations::*;
use merge::merge_layer;

#[cfg(test)]
mod merge_tests;
#[cfg(test)]
mod registry_tests;

struct PreparedWrite {
    destination: PathBuf,
    temporary: PathBuf,
    backup: PathBuf,
    had_original: bool,
    cleanup_on_drop: bool,
}

impl PreparedWrite {
    fn new(destination: PathBuf, bytes: Vec<u8>) -> Result<Self, ConfigError> {
        let parent = destination
            .parent()
            .ok_or_else(|| ConfigError::InvalidPath(destination.clone()))?;
        ensure_private_directory(parent)?;
        let token = random_sidecar_token()?;
        let temporary = parent.join(format!(".config.{token}.tmp"));
        let backup = parent.join(format!(".config.{token}.bak"));
        let mut file = create_private_file(&temporary).map_err(|source| ConfigError::Io {
            path: temporary.clone(),
            source,
        })?;
        let prepared = Self {
            had_original: destination.is_file(),
            destination,
            temporary,
            backup,
            cleanup_on_drop: true,
        };
        file.write_all(&bytes)
            .and_then(|()| file.write_all(b"\n"))
            .and_then(|()| file.sync_all())
            .map_err(|source| ConfigError::Io {
                path: prepared.temporary.clone(),
                source,
            })?;
        Ok(prepared)
    }

    fn journal_entry(&self) -> JournalEntry {
        JournalEntry {
            destination: self.destination.clone(),
            temporary: self.temporary.clone(),
            backup: self.backup.clone(),
            had_original: self.had_original,
        }
    }
}

impl Drop for PreparedWrite {
    fn drop(&mut self) {
        if self.cleanup_on_drop {
            let _ = fs::remove_file(&self.temporary);
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum JournalState {
    Prepared,
    Committed,
}

#[derive(Debug, Serialize, Deserialize)]
struct ConfigJournal {
    state: JournalState,
    entries: Vec<JournalEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct JournalEntry {
    destination: PathBuf,
    temporary: PathBuf,
    backup: PathBuf,
    had_original: bool,
}

fn commit_prepared(prepared: &[PreparedWrite]) -> Result<(), ConfigError> {
    for item in prepared {
        if item.had_original {
            fs::rename(&item.destination, &item.backup).map_err(|source| ConfigError::Io {
                path: item.destination.clone(),
                source,
            })?;
        }
        fs::rename(&item.temporary, &item.destination).map_err(|source| ConfigError::Io {
            path: item.destination.clone(),
            source,
        })?;
        if let Some(parent) = item.destination.parent() {
            sync_directory(parent)?;
        }
    }
    Ok(())
}

fn rollback_prepared(prepared: &[PreparedWrite]) -> Result<(), ConfigError> {
    for item in prepared.iter().rev() {
        if item.backup.exists() {
            if item.destination.exists() {
                fs::remove_file(&item.destination).map_err(|source| ConfigError::Io {
                    path: item.destination.clone(),
                    source,
                })?;
            }
            fs::rename(&item.backup, &item.destination).map_err(|source| ConfigError::Io {
                path: item.destination.clone(),
                source,
            })?;
        } else if !item.had_original && item.destination.exists() {
            fs::remove_file(&item.destination).map_err(|source| ConfigError::Io {
                path: item.destination.clone(),
                source,
            })?;
        }
        if item.temporary.exists() {
            fs::remove_file(&item.temporary).map_err(|source| ConfigError::Io {
                path: item.temporary.clone(),
                source,
            })?;
        }
        if let Some(parent) = item.destination.parent() {
            sync_directory(parent)?;
        }
    }
    Ok(())
}

fn cleanup_prepared(prepared: &[PreparedWrite]) -> Result<(), ConfigError> {
    for item in prepared {
        if item.backup.exists() {
            fs::remove_file(&item.backup).map_err(|source| ConfigError::Io {
                path: item.backup.clone(),
                source,
            })?;
        }
        if item.temporary.exists() {
            fs::remove_file(&item.temporary).map_err(|source| ConfigError::Io {
                path: item.temporary.clone(),
                source,
            })?;
        }
    }
    Ok(())
}

fn recover_transaction(paths: &ConfigPaths) -> Result<(), ConfigError> {
    let path = paths.vibe_home.join(TRANSACTION_FILE);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(ConfigError::Io {
                path: path.clone(),
                source,
            });
        }
    };
    let journal: ConfigJournal =
        serde_json::from_slice(&bytes).map_err(|source| ConfigError::CorruptJournal {
            path: path.clone(),
            source,
        })?;
    validate_journal(&journal, paths)?;
    match journal.state {
        JournalState::Prepared => {
            let recovered = journal
                .entries
                .into_iter()
                .map(|entry| PreparedWrite {
                    destination: entry.destination,
                    temporary: entry.temporary,
                    backup: entry.backup,
                    had_original: entry.had_original,
                    cleanup_on_drop: false,
                })
                .collect::<Vec<_>>();
            rollback_prepared(&recovered)?;
        }
        JournalState::Committed => {
            for entry in journal.entries {
                if entry.backup.exists() {
                    fs::remove_file(&entry.backup).map_err(|source| ConfigError::Io {
                        path: entry.backup.clone(),
                        source,
                    })?;
                }
                if entry.temporary.exists() {
                    fs::remove_file(&entry.temporary).map_err(|source| ConfigError::Io {
                        path: entry.temporary.clone(),
                        source,
                    })?;
                }
            }
        }
    }
    fs::remove_file(&path).map_err(|source| ConfigError::Io {
        path: path.clone(),
        source,
    })?;
    sync_directory(&paths.vibe_home)
}

fn cleanup_orphan_sidecars(paths: &ConfigPaths) -> Result<(), ConfigError> {
    let parents = [paths.user_config(), paths.project_config()]
        .into_iter()
        .filter_map(|path| path.parent().map(Path::to_path_buf))
        .collect::<BTreeSet<_>>();
    for parent in parents {
        let entries = match fs::read_dir(&parent) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(source) => {
                return Err(ConfigError::Io {
                    path: parent,
                    source,
                });
            }
        };
        let mut removed = false;
        for entry in entries {
            let entry = entry.map_err(|source| ConfigError::Io {
                path: parent.clone(),
                source,
            })?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let token = name.strip_prefix(".config.").and_then(|name| {
                name.strip_suffix(".tmp")
                    .or_else(|| name.strip_suffix(".bak"))
            });
            if token.is_none_or(|token| {
                token.len() != 32 || !token.bytes().all(|byte| byte.is_ascii_hexdigit())
            }) {
                continue;
            }
            fs::remove_file(entry.path()).map_err(|source| ConfigError::Io {
                path: entry.path(),
                source,
            })?;
            removed = true;
        }
        if removed {
            sync_directory(&parent)?;
        }
    }
    Ok(())
}

pub(super) fn random_sidecar_token() -> Result<String, ConfigError> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(|_| ConfigError::RandomUnavailable)?;
    Ok(format!("{:032x}", u128::from_ne_bytes(bytes)))
}

fn validate_journal(journal: &ConfigJournal, paths: &ConfigPaths) -> Result<(), ConfigError> {
    let allowed = [paths.user_config(), paths.project_config()];
    for entry in &journal.entries {
        if !allowed.contains(&entry.destination)
            || entry.temporary == entry.backup
            || !transaction_sidecar(&entry.temporary, &entry.destination, "tmp")
            || !transaction_sidecar(&entry.backup, &entry.destination, "bak")
        {
            return Err(ConfigError::UnsafeJournal(path_for_journal(paths)));
        }
    }
    Ok(())
}

fn path_for_journal(paths: &ConfigPaths) -> PathBuf {
    paths.vibe_home.join(TRANSACTION_FILE)
}

fn transaction_sidecar(path: &Path, destination: &Path, suffix: &str) -> bool {
    if path.parent() != destination.parent() {
        return false;
    }
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with(".config.") && name.ends_with(&format!(".{suffix}")))
}

fn write_journal(path: &Path, journal: &ConfigJournal) -> Result<(), ConfigError> {
    let parent = path
        .parent()
        .ok_or_else(|| ConfigError::InvalidPath(path.to_path_buf()))?;
    ensure_private_directory(parent)?;
    let mut encoded = serde_json::to_vec(journal).map_err(ConfigError::Json)?;
    encoded.push(b'\n');
    write_atomically(path, "journal", &encoded).map_err(ConfigError::from)
}

fn read_table_optional(path: &Path) -> Result<Table, ConfigError> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Table::new()),
        Err(source) => {
            return Err(ConfigError::Io {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    let metadata = file.metadata().map_err(|source| ConfigError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if metadata.len() > MAX_CONFIG_BYTES {
        return Err(ConfigError::TooLarge(path.to_path_buf()));
    }
    let mut contents = String::new();
    file.read_to_string(&mut contents)
        .map_err(|source| ConfigError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    if contents.len() as u64 > MAX_CONFIG_BYTES {
        return Err(ConfigError::TooLarge(path.to_path_buf()));
    }
    contents
        .parse::<Table>()
        .map_err(|source| ConfigError::InvalidToml {
            path: path.to_path_buf(),
            source,
        })
}

fn fingerprint_optional(path: &Path) -> Result<Option<String>, ConfigError> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(ConfigError::Io {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|source| ConfigError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    if bytes.len() as u64 > MAX_CONFIG_BYTES {
        return Err(ConfigError::TooLarge(path.to_path_buf()));
    }
    Ok(Some(hex_digest(&bytes)))
}

fn hex_digest(bytes: &[u8]) -> String {
    hex_encode(&Sha256::digest(bytes))
}

fn environment_table(environment: &BTreeMap<String, String>) -> Result<Table, ConfigError> {
    let mut table = Table::new();
    for (key, raw) in environment {
        let Some(name) = key.strip_prefix("VIBE_") else {
            continue;
        };
        if raw.is_empty() {
            continue;
        }
        let path = name
            .split("__")
            .map(str::to_ascii_lowercase)
            .collect::<Vec<_>>();
        if path.iter().any(String::is_empty) {
            return Err(ConfigError::InvalidEnvironmentKey(key.clone()));
        }
        let parsed = format!("value = {raw}")
            .parse::<Table>()
            .ok()
            .and_then(|mut parsed| parsed.remove("value"))
            .unwrap_or_else(|| Value::String(raw.clone()));
        apply_mutation(
            &mut table,
            &ConfigMutation {
                path,
                value: Some(parsed),
            },
        )?;
    }
    Ok(table)
}

fn apply_mutation(table: &mut Table, mutation: &ConfigMutation) -> Result<(), ConfigError> {
    let (last, parents) = mutation
        .path
        .split_last()
        .ok_or(ConfigError::EmptyMutationPath)?;
    let mut cursor = table;
    for segment in parents {
        let value = cursor
            .entry(segment.clone())
            .or_insert_with(|| Value::Table(Table::new()));
        cursor = value
            .as_table_mut()
            .ok_or_else(|| ConfigError::NonTableParent(segment.clone()))?;
    }
    match &mutation.value {
        Some(value) => {
            cursor.insert(last.clone(), value.clone());
        }
        None => {
            cursor.remove(last);
        }
    }
    Ok(())
}

fn validate_table(table: &Table) -> Result<(), ConfigError> {
    validate_urls(table, &mut Vec::new())
}

fn validate_urls(table: &Table, path: &mut Vec<String>) -> Result<(), ConfigError> {
    for (key, value) in table {
        path.push(key.clone());
        if is_proxy_key(key)
            && let Some(raw) = value.as_str()
        {
            let parsed = Url::parse(raw).map_err(|_| ConfigError::InvalidSensitiveUrl {
                path: path.join("."),
            })?;
            if !parsed.username().is_empty() || parsed.password().is_some() {
                return Err(ConfigError::SensitiveUrlCredentials {
                    path: path.join("."),
                });
            }
        }
        if let Some(nested) = value.as_table() {
            validate_urls(nested, path)?;
        }
        path.pop();
    }
    Ok(())
}

fn is_proxy_key(key: &str) -> bool {
    matches!(
        key.to_ascii_lowercase().as_str(),
        "proxy" | "http_proxy" | "https_proxy" | "proxy_url"
    )
}

fn redact_table(table: &Table) -> JsonValue {
    let mut object = serde_json::Map::new();
    for (key, value) in table {
        let redacted = if is_sensitive_key(key) {
            JsonValue::String("[redacted]".to_owned())
        } else {
            redact_value(value)
        };
        object.insert(key.clone(), redacted);
    }
    JsonValue::Object(object)
}

fn redact_value(value: &Value) -> JsonValue {
    match value {
        Value::String(value) => JsonValue::String(value.clone()),
        Value::Integer(value) => JsonValue::from(*value),
        Value::Float(value) => JsonValue::from(*value),
        Value::Boolean(value) => JsonValue::from(*value),
        Value::Datetime(value) => JsonValue::String(value.to_string()),
        Value::Array(values) => JsonValue::Array(values.iter().map(redact_value).collect()),
        Value::Table(values) => redact_table(values),
    }
}

fn is_sensitive_key(key: &str) -> bool {
    let normalized = key
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .flat_map(char::to_lowercase)
        .collect::<String>();
    normalized.contains("password")
        || normalized.contains("secret")
        || normalized.contains("token")
        || normalized.contains("apikey")
        || normalized.contains("authorization")
        || normalized.contains("credential")
        || normalized.contains("privatekey")
        || normalized.contains("accesskey")
        || normalized.contains("proxy")
}

struct ConfigFileLock {
    file: File,
}

impl ConfigFileLock {
    fn acquire(vibe_home: &Path) -> Result<Self, ConfigError> {
        use fs2::FileExt as _;

        let path = vibe_home.join(LOCK_FILE);
        let file = atomic_file::open_private_lock(&path).map_err(|source| ConfigError::Io {
            path: path.clone(),
            source,
        })?;
        file.lock_exclusive().map_err(|source| ConfigError::Io {
            path: path.clone(),
            source,
        })?;
        Ok(Self { file })
    }
}

impl Drop for ConfigFileLock {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.file);
    }
}

fn ensure_private_directory(path: &Path) -> Result<(), ConfigError> {
    atomic_file::ensure_private_directory(path).map_err(|source| ConfigError::Io {
        path: path.to_path_buf(),
        source,
    })
}

fn sync_directory(path: &Path) -> Result<(), ConfigError> {
    atomic_file::sync_directory(path).map_err(|source| ConfigError::Io {
        path: path.to_path_buf(),
        source,
    })
}

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
    #[error("configuration changed concurrently for target `{target:?}`")]
    ConcurrentEdit { target: ConfigTarget },
    #[error(
        "configuration commit failed (`{commit}`) and rollback also failed (`{rollback}`); recovery journal retained"
    )]
    RollbackFailed { commit: String, rollback: String },
    #[error("configuration mutation path must not be empty")]
    EmptyMutationPath,
    #[error("configuration path component `{0}` is not a table")]
    NonTableParent(String),
    #[error("invalid VIBE environment key `{0}`")]
    InvalidEnvironmentKey(String),
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
        config.experiments = table("thinking = \"low\"");
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
        };
        let error = snapshot
            .mcp_servers(Path::new("/workspace"))
            .expect_err("unsupported transport fails closed")
            .to_string();
        assert!(error.contains("must be stdio or streamable-http"));
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
            entries: vec![JournalEntry {
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
