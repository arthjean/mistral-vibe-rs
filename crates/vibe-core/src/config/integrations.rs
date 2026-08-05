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

    pub(super) fn invalid(self, message: &str) -> ConfigError {
        match self {
            Self::McpServers => ConfigError::InvalidMcp(message.to_owned()),
            Self::Connectors => ConfigError::InvalidIntegration(message.to_owned()),
        }
    }
}
