use super::*;

pub(super) fn config_array(
    table: &Table,
    collection: IntegrationCollection,
) -> Result<Vec<Value>, ConfigError> {
    match table.get(collection.key()) {
        None => Ok(Vec::new()),
        Some(Value::Array(entries)) => Ok(entries.clone()),
        Some(_) => {
            Err(collection.invalid(&format!("{} must be an array of tables", collection.key())))
        }
    }
}

pub(super) fn config_array_for_target(
    snapshot: &ConfigSnapshot,
    target: ConfigTarget,
    collection: IntegrationCollection,
) -> Result<Vec<Value>, ConfigError> {
    snapshot
        .target_values
        .get(&target)
        .map_or_else(|| Ok(Vec::new()), |table| config_array(table, collection))
}

pub(super) fn required_mcp_string<'a>(table: &'a Table, key: &str) -> Result<&'a str, ConfigError> {
    optional_mcp_string(table, key)?
        .ok_or_else(|| ConfigError::InvalidMcp(format!("MCP server field `{key}` is required")))
}

pub(super) fn optional_mcp_string<'a>(
    table: &'a Table,
    key: &str,
) -> Result<Option<&'a str>, ConfigError> {
    match table.get(key) {
        Some(Value::String(value)) if !value.is_empty() => Ok(Some(value)),
        Some(_) => Err(ConfigError::InvalidMcp(format!(
            "MCP server field `{key}` must be a non-empty string"
        ))),
        None => Ok(None),
    }
}

pub(super) fn optional_mcp_bool(table: &Table, key: &str) -> Result<Option<bool>, ConfigError> {
    match table.get(key) {
        Some(Value::Boolean(value)) => Ok(Some(*value)),
        Some(_) => Err(ConfigError::InvalidMcp(format!(
            "MCP server field `{key}` must be a boolean"
        ))),
        None => Ok(None),
    }
}

pub(super) fn optional_mcp_strings(table: &Table, key: &str) -> Result<Vec<String>, ConfigError> {
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

pub(super) fn optional_mcp_environment_at(
    table: &Table,
    key: &str,
) -> Result<BTreeMap<String, String>, ConfigError> {
    match table.get(key) {
        Some(Value::Table(values)) => values
            .iter()
            .map(|(name, value)| {
                value
                    .as_str()
                    .map(|value| (name.clone(), value.to_owned()))
                    .ok_or_else(|| {
                        ConfigError::InvalidMcp(format!(
                            "MCP server field `{key}` must contain only strings"
                        ))
                    })
            })
            .collect(),
        Some(_) => Err(ConfigError::InvalidMcp(format!(
            "MCP server field `{key}` must be a table"
        ))),
        None => Ok(BTreeMap::new()),
    }
}

pub(super) fn optional_mcp_timeout(
    table: &Table,
    key: &str,
    default_ms: u64,
) -> Result<u64, ConfigError> {
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

/// A configured collection of external tool providers.
///
/// MCP servers and connectors share one persistence shape: an array of tables
/// keyed by an identity, each carrying `disabled` and `disabled_tools`. The
/// differences between them live here rather than at every call site.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum IntegrationCollection {
    McpServers,
    Connectors,
}

impl IntegrationCollection {
    pub(super) const fn key(self) -> &'static str {
        match self {
            Self::McpServers => "mcp_servers",
            Self::Connectors => "connectors",
        }
    }

    /// The field an entry is identified by, which connectors allow to be `id`.
    pub(super) fn identity(self, table: &Table) -> Option<&str> {
        match self {
            Self::McpServers => table.get("name"),
            Self::Connectors => table.get("name").or_else(|| table.get("id")),
        }
        .and_then(Value::as_str)
    }

    /// The identity an entry is matched under.
    ///
    /// An MCP server is read under its normalized alias, so it has to be
    /// addressed under that one too: a persisted `my server` answers to
    /// `my_server`, which is the only name any reader ever sees.
    pub(super) fn identity_key(self, table: &Table) -> Option<String> {
        let identity = self.identity(table)?;
        Some(match self {
            Self::McpServers => super::mcp::normalize_mcp_server_name(identity),
            Self::Connectors => identity.to_owned(),
        })
    }

    /// How this collection names a field in a diagnostic.
    const fn field_label(self) -> &'static str {
        match self {
            Self::McpServers => "MCP server field",
            Self::Connectors => "connector field",
        }
    }

    /// The enablement one entry declares.
    ///
    /// `disabled` and `disabled_tools` are the pair this collection's doc names
    /// as shared, so both collections read them here rather than each spelling
    /// out the same two shapes with its own error vocabulary.
    pub(super) fn preference(
        self,
        entry: &Table,
    ) -> Result<super::IntegrationPreference, ConfigError> {
        let label = self.field_label();
        let enabled = match entry.get("disabled") {
            None => true,
            Some(Value::Boolean(disabled)) => !disabled,
            Some(_) => return Err(self.invalid(&format!("{label} `disabled` must be a boolean"))),
        };
        let disabled_tools = match entry.get("disabled_tools") {
            None => BTreeSet::new(),
            Some(Value::Array(values)) => values
                .iter()
                .map(|value| {
                    value.as_str().map(str::to_owned).ok_or_else(|| {
                        self.invalid(&format!("{label} `disabled_tools` must contain strings"))
                    })
                })
                .collect::<Result<_, _>>()?,
            Some(_) => {
                return Err(self.invalid(&format!("{label} `disabled_tools` must be an array")));
            }
        };
        Ok(super::IntegrationPreference {
            enabled,
            disabled_tools,
        })
    }

    pub(super) fn invalid(self, message: &str) -> ConfigError {
        match self {
            Self::McpServers => ConfigError::InvalidMcp(message.to_owned()),
            Self::Connectors => ConfigError::InvalidIntegration(message.to_owned()),
        }
    }
}

// --------------------------------------------------------------------------
// Persistence
// --------------------------------------------------------------------------

/// Reading and writing the two collections against the file writes land in.
///
/// These sit here rather than on the store's own module because every one of
/// them is expressed in the vocabulary [`IntegrationCollection`] declares: the
/// identity an entry is keyed by, the enablement pair it carries, and the array
/// it lives in. The store contributes the layering and the transaction; this
/// contributes what an entry means.
impl super::LayeredConfig {
    pub fn preflight_mcp_add(&self, config: &McpServerConfig) -> Result<(), ConfigError> {
        let snapshot = self.load()?;
        preflight_mcp_add(&snapshot.effective, config)
    }

    pub fn persist_mcp_add(&self, config: &McpServerConfig) -> Result<ConfigSnapshot, ConfigError> {
        let snapshot = self.load()?;
        preflight_mcp_add(&snapshot.effective, config)?;
        let collection = IntegrationCollection::McpServers;
        let target = snapshot.selected_target;
        // Rejects a target whose `mcp_servers` is not a list before the upsert
        // decides to append to it.
        config_array_for_target(&snapshot, target, collection)?;
        let mutation = patch::resolve_upsert(
            snapshot
                .target_values
                .get(&target)
                .and_then(|values| values.get(collection.key())),
            &JsonPointer::from_segments([collection.key()]),
            "name",
            mcp_server_table(config)?,
        );
        self.batch_write(&[ConfigWrite {
            target,
            expected_fingerprint: snapshot.fingerprints.get(&target).cloned().flatten(),
            mutations: vec![mutation],
        }])
    }

    /// Drops the entry named `name` from the file writes land in.
    ///
    /// A name no entry carries is reported as not removed rather than raised:
    /// asking twice is not an error, and the second answer says so.
    pub fn persist_mcp_remove(&self, name: &str) -> Result<mcp::McpRemoval, ConfigError> {
        let name = mcp::normalize_mcp_server_name(name);
        if name.is_empty() {
            return Err(ConfigError::InvalidMcp(
                "MCP server name must contain letters or numbers".to_owned(),
            ));
        }
        let snapshot = self.load()?;
        let collection = IntegrationCollection::McpServers;
        let target = snapshot.selected_target;
        let entries = config_array_for_target(&snapshot, target, collection)?;
        let retained = entries
            .iter()
            .filter(|entry| {
                entry
                    .as_table()
                    .and_then(|entry| collection.identity(entry))
                    .map(mcp::normalize_mcp_server_name)
                    .as_deref()
                    != Some(name.as_str())
            })
            .cloned()
            .collect::<Vec<_>>();
        if retained.len() == entries.len() {
            return Ok(mcp::McpRemoval {
                name,
                removed: false,
            });
        }
        self.replace_array_cas(
            target,
            snapshot.fingerprints.get(&target).cloned().flatten(),
            collection.key(),
            retained,
        )?;
        Ok(mcp::McpRemoval {
            name,
            removed: true,
        })
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
        let entries =
            integration_entries(&snapshot, collection, name, enabled, disabled_tools, target)?;
        self.replace_array_cas(target, expected_fingerprint, collection.key(), entries)
    }

    /// Persists enablement for one entry, failing if the target changed.
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
        let entries =
            integration_entries(&snapshot, collection, name, enabled, disabled_tools, target)?;
        self.replace_array_cas(target, expected_fingerprint, collection.key(), entries)
    }

    pub fn connector_preferences(
        &self,
    ) -> Result<BTreeMap<String, IntegrationPreference>, ConfigError> {
        let snapshot = self.load()?;
        let entries = config_array(&snapshot.effective, IntegrationCollection::Connectors)?;
        let collection = IntegrationCollection::Connectors;
        let mut preferences = BTreeMap::new();
        for entry in entries {
            let entry = entry.as_table().ok_or_else(|| {
                ConfigError::InvalidIntegration("each connectors entry must be a table".to_owned())
            })?;
            let name = collection
                .identity(entry)
                .filter(|name| !name.is_empty())
                .ok_or_else(|| {
                    ConfigError::InvalidIntegration(
                        "connector field `name` must be a non-empty string".to_owned(),
                    )
                })?;
            if preferences
                .insert(name.to_owned(), collection.preference(entry)?)
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
}

/// The `collection` array `target` should hold once `name` carries `enabled`
/// and `disabled_tools`.
///
/// Read from one snapshot rather than one per caller: the fingerprint a write
/// is guarded by and the entries it writes describe the same read, so a file
/// edited between the two cannot be written over with a stale array.
///
/// The entry is created in the target when it is only present in another layer,
/// so disabling a default-provided server writes an explicit record.
fn integration_entries(
    snapshot: &ConfigSnapshot,
    collection: IntegrationCollection,
    name: &str,
    enabled: bool,
    disabled_tools: &BTreeSet<String>,
    target: ConfigTarget,
) -> Result<Vec<Value>, ConfigError> {
    if collection == IntegrationCollection::McpServers
        && !config_array(&snapshot.effective, collection)?
            .iter()
            .filter_map(Value::as_table)
            .any(|entry| collection.identity_key(entry).as_deref() == Some(name))
    {
        return Err(collection.invalid(&format!("unknown MCP server `{name}`")));
    }
    let mut entries = config_array_for_target(snapshot, target, collection)?;
    let position = entries
        .iter()
        .position(|entry| {
            entry
                .as_table()
                .and_then(|entry| collection.identity_key(entry))
                .as_deref()
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
    Ok(entries)
}
