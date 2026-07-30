use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::{Value as JsonValue, json};
use sha2::{Digest, Sha256};
use thiserror::Error;
use toml::{Table, Value};
use url::Url;

use crate::mcp::{
    DEFAULT_MCP_STARTUP_TIMEOUT_MS, DEFAULT_MCP_TOOL_TIMEOUT_MS, McpServerConfig,
    McpTransportConfig,
};

const CONFIG_FILE: &str = "config.toml";
const PROJECT_DIRECTORY: &str = ".vibe";
const TRANSACTION_FILE: &str = ".config-transaction.json";
const LOCK_FILE: &str = ".config.lock";
const MAX_CONFIG_BYTES: u64 = 4 * 1024 * 1024;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(1);

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

#[derive(Debug, Clone, PartialEq)]
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
    pub layers: Vec<ConfigLayerKind>,
}

impl ConfigSnapshot {
    #[must_use]
    pub fn public_view(&self) -> JsonValue {
        json!({
            "config": redact_table(&self.effective),
            "selectedTarget": self.selected_target,
            "selectedPath": self.selected_path,
            "fingerprints": self.fingerprints,
            "layers": self.layers,
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
            if transport != "stdio" {
                return Err(ConfigError::InvalidMcp(
                    "only stdio MCP transport is supported in TOML configuration".to_owned(),
                ));
            }
            let alias = required_mcp_string(table, "name")?.to_owned();
            if !aliases.insert(alias.clone()) {
                return Err(ConfigError::InvalidMcp(
                    "MCP server names must be unique".to_owned(),
                ));
            }
            let command = required_mcp_string(table, "command")?.to_owned();
            let arguments = optional_mcp_strings(table, "args")?;
            let environment = optional_mcp_environment(table)?;
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
            let disabled = optional_mcp_bool(table, "disabled")?.unwrap_or(false);
            let disabled_tools = optional_mcp_strings(table, "disabled_tools")?
                .into_iter()
                .collect();
            servers.push(McpServerConfig {
                alias,
                transport: McpTransportConfig::Stdio {
                    command,
                    arguments,
                    environment,
                    working_directory,
                },
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
                oauth: None,
            });
        }
        Ok(servers)
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

    pub fn load(&self) -> Result<ConfigSnapshot, ConfigError> {
        let _guard = self
            .transaction_lock
            .lock()
            .map_err(|_| ConfigError::LockPoisoned)?;
        ensure_private_directory(&self.paths.vibe_home)?;
        let _file_guard = ConfigFileLock::acquire(&self.paths.vibe_home)?;
        recover_transaction(&self.paths)?;

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

        let selected = read_table_optional(&selected_path)?;
        let environment = environment_table(&self.environment)?;
        let layers = vec![
            ConfigLayer {
                kind: ConfigLayerKind::Defaults,
                values: self.defaults.clone(),
            },
            ConfigLayer {
                kind: ConfigLayerKind::SelectedToml,
                values: selected,
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
            merge_tables(&mut effective, &layer.values);
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
            layers: layers.into_iter().map(|layer| layer.kind).collect(),
        })
    }

    pub fn reload(&self) -> Result<ConfigSnapshot, ConfigError> {
        self.load()
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

    #[must_use]
    pub fn schema() -> JsonValue {
        json!({
            "type": "object",
            "additionalProperties": true,
            "properties": {
                "active_model": {"type": "string"},
                "thinking": {"enum": ["off", "low", "medium", "high"]},
                "proxy": {"type": ["string", "null"], "format": "uri"},
                "tls_ca_path": {"type": ["string", "null"]},
                "dotenv_path": {"type": ["string", "null"]},
                "mcp_servers": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "additionalProperties": true,
                        "required": ["name", "transport", "command"],
                        "properties": {
                            "name": {"type": "string", "minLength": 1},
                            "transport": {"const": "stdio"},
                            "command": {"type": "string", "minLength": 1},
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

struct PreparedWrite {
    destination: PathBuf,
    temporary: PathBuf,
    backup: PathBuf,
    had_original: bool,
}

impl PreparedWrite {
    fn new(destination: PathBuf, bytes: Vec<u8>) -> Result<Self, ConfigError> {
        let parent = destination
            .parent()
            .ok_or_else(|| ConfigError::InvalidPath(destination.clone()))?;
        ensure_private_directory(parent)?;
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temporary = parent.join(format!(".config.{sequence}.tmp"));
        let backup = parent.join(format!(".config.{sequence}.bak"));
        let mut file = open_private_new(&temporary).map_err(|source| ConfigError::Io {
            path: temporary.clone(),
            source,
        })?;
        file.write_all(&bytes)
            .and_then(|()| file.write_all(b"\n"))
            .and_then(|()| file.sync_all())
            .map_err(|source| ConfigError::Io {
                path: temporary.clone(),
                source,
            })?;
        Ok(Self {
            had_original: destination.is_file(),
            destination,
            temporary,
            backup,
        })
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
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temporary = parent.join(format!(".journal.{sequence}.tmp"));
    let encoded = serde_json::to_vec(journal).map_err(ConfigError::Json)?;
    let result = (|| {
        let mut file = open_private_new(&temporary).map_err(|source| ConfigError::Io {
            path: temporary.clone(),
            source,
        })?;
        file.write_all(&encoded)
            .and_then(|()| file.write_all(b"\n"))
            .and_then(|()| file.sync_all())
            .map_err(|source| ConfigError::Io {
                path: temporary.clone(),
                source,
            })?;
        fs::rename(&temporary, path).map_err(|source| ConfigError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        sync_directory(parent)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
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
    let digest = Sha256::digest(bytes);
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(encoded, "{byte:02x}");
    }
    encoded
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

fn required_mcp_string<'a>(table: &'a Table, key: &str) -> Result<&'a str, ConfigError> {
    optional_mcp_string(table, key)?
        .ok_or_else(|| ConfigError::InvalidMcp(format!("MCP server field `{key}` is required")))
}

fn optional_mcp_string<'a>(table: &'a Table, key: &str) -> Result<Option<&'a str>, ConfigError> {
    match table.get(key) {
        Some(Value::String(value)) if !value.is_empty() => Ok(Some(value)),
        Some(_) => Err(ConfigError::InvalidMcp(format!(
            "MCP server field `{key}` must be a non-empty string"
        ))),
        None => Ok(None),
    }
}

fn optional_mcp_bool(table: &Table, key: &str) -> Result<Option<bool>, ConfigError> {
    match table.get(key) {
        Some(Value::Boolean(value)) => Ok(Some(*value)),
        Some(_) => Err(ConfigError::InvalidMcp(format!(
            "MCP server field `{key}` must be a boolean"
        ))),
        None => Ok(None),
    }
}

fn optional_mcp_strings(table: &Table, key: &str) -> Result<Vec<String>, ConfigError> {
    match table.get(key) {
        Some(Value::Array(values)) => values
            .iter()
            .map(|value| {
                value.as_str().map(str::to_owned).ok_or_else(|| {
                    ConfigError::InvalidMcp(format!(
                        "MCP server field `{key}` must contain only strings"
                    ))
                })
            })
            .collect(),
        Some(_) => Err(ConfigError::InvalidMcp(format!(
            "MCP server field `{key}` must be an array"
        ))),
        None => Ok(Vec::new()),
    }
}

fn optional_mcp_environment(table: &Table) -> Result<BTreeMap<String, String>, ConfigError> {
    match table.get("env") {
        Some(Value::Table(values)) => values
            .iter()
            .map(|(key, value)| {
                value
                    .as_str()
                    .map(|value| (key.clone(), value.to_owned()))
                    .ok_or_else(|| {
                        ConfigError::InvalidMcp(
                            "MCP server field `env` must contain only strings".to_owned(),
                        )
                    })
            })
            .collect(),
        Some(_) => Err(ConfigError::InvalidMcp(
            "MCP server field `env` must be a table".to_owned(),
        )),
        None => Ok(BTreeMap::new()),
    }
}

fn optional_mcp_timeout(table: &Table, key: &str, default_ms: u64) -> Result<u64, ConfigError> {
    let Some(value) = table.get(key) else {
        return Ok(default_ms);
    };
    let seconds = match value {
        Value::Integer(value) => *value as f64,
        Value::Float(value) => *value,
        _ => {
            return Err(ConfigError::InvalidMcp(format!(
                "MCP server field `{key}` must be numeric"
            )));
        }
    };
    let milliseconds = seconds * 1_000.0;
    if !milliseconds.is_finite()
        || !(1.0..=600_000.0).contains(&milliseconds)
        || milliseconds.fract() != 0.0
    {
        return Err(ConfigError::InvalidMcp(format!(
            "MCP server field `{key}` must resolve to 1 through 600000 milliseconds"
        )));
    }
    Ok(milliseconds as u64)
}

fn merge_tables(target: &mut Table, overlay: &Table) {
    for (key, value) in overlay {
        match (target.get_mut(key), value) {
            (Some(Value::Table(target_table)), Value::Table(overlay_table)) => {
                merge_tables(target_table, overlay_table);
            }
            _ => {
                target.insert(key.clone(), value.clone());
            }
        }
    }
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
        let file = open_private_lock(&path).map_err(|source| ConfigError::Io {
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
    fs::create_dir_all(path).map_err(|source| ConfigError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|source| {
            ConfigError::Io {
                path: path.to_path_buf(),
                source,
            }
        })?;
    }
    Ok(())
}

fn open_private_new(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;

        options.mode(0o600);
    }
    options.open(path)
}

fn open_private_lock(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;

        options.mode(0o600);
    }
    options.open(path)
}

fn sync_directory(path: &Path) -> Result<(), ConfigError> {
    #[cfg(unix)]
    {
        File::open(path)
            .and_then(|directory| directory.sync_all())
            .map_err(|source| ConfigError::Io {
                path: path.to_path_buf(),
                source,
            })
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("configuration state lock is poisoned")]
    LockPoisoned,
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
    #[error("invalid MCP configuration: {0}")]
    InvalidMcp(String),
    #[error("invalid URL in sensitive configuration field `{path}`")]
    InvalidSensitiveUrl { path: String },
    #[error("credentials are forbidden in sensitive configuration field `{path}`")]
    SensitiveUrlCredentials { path: String },
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

        let revoked = config
            .with_project_trusted(false)
            .load()
            .expect("user fallback");
        assert_eq!(revoked.selected_target, ConfigTarget::User);
        assert_ne!(revoked.effective["active_model"].as_str(), Some("project"));
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
            layers: Vec::new(),
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
            layers: Vec::new(),
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
            layers: Vec::new(),
        };
        let error = snapshot
            .mcp_servers(Path::new("/workspace"))
            .expect_err("unsupported transport fails closed")
            .to_string();
        assert!(error.contains("only stdio"));
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
