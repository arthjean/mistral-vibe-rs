use super::*;
use crate::text::canonical_url;

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

pub(super) fn preflight_mcp_add(
    table: &Table,
    config: &McpServerConfig,
) -> Result<(), ConfigError> {
    for entry in config_array(table, IntegrationCollection::McpServers)? {
        let entry = entry.as_table().ok_or_else(|| {
            ConfigError::InvalidMcp("each mcp_servers entry must be a table".to_owned())
        })?;
        if entry.get("name").and_then(Value::as_str) == Some(&config.alias) {
            return Err(ConfigError::InvalidMcp(format!(
                "MCP server name `{}` is already configured",
                config.alias
            )));
        }
        let Some(new_url) = mcp_transport_url(&config.transport) else {
            continue;
        };
        let Some(existing_url) = entry.get("url").and_then(Value::as_str) else {
            continue;
        };
        if Url::parse(existing_url)
            .ok()
            .is_some_and(|url| canonical_url(&url) == canonical_url(new_url))
        {
            return Err(ConfigError::InvalidMcp(
                "an MCP server with this URL is already configured".to_owned(),
            ));
        }
    }
    Ok(())
}

pub(super) fn mcp_transport_url(transport: &McpTransportConfig) -> Option<&Url> {
    match transport {
        McpTransportConfig::StreamableHttp { url, .. } => Some(url),
        McpTransportConfig::Stdio { .. } => None,
    }
}

pub(super) fn mcp_server_table(config: &McpServerConfig) -> Result<Table, ConfigError> {
    let mut table = Table::new();
    table.insert("name".to_owned(), Value::String(config.alias.clone()));
    match &config.transport {
        McpTransportConfig::Stdio {
            command,
            arguments,
            environment,
            working_directory,
        } => {
            table.insert("transport".to_owned(), Value::String("stdio".to_owned()));
            table.insert("command".to_owned(), Value::String(command.clone()));
            if !arguments.is_empty() {
                table.insert(
                    "args".to_owned(),
                    Value::Array(arguments.iter().cloned().map(Value::String).collect()),
                );
            }
            if !environment.is_empty() {
                table.insert(
                    "env".to_owned(),
                    Value::Table(
                        environment
                            .iter()
                            .map(|(key, value)| (key.clone(), Value::String(value.clone())))
                            .collect(),
                    ),
                );
            }
            if let Some(working_directory) = working_directory {
                table.insert(
                    "cwd".to_owned(),
                    Value::String(working_directory.to_string_lossy().into_owned()),
                );
            }
        }
        McpTransportConfig::StreamableHttp { url, headers } => {
            table.insert(
                "transport".to_owned(),
                Value::String("streamable-http".to_owned()),
            );
            insert_http_mcp_fields(&mut table, url, headers);
        }
    }
    table.insert("disabled".to_owned(), Value::Boolean(!config.enabled));
    table.insert(
        "disabled_tools".to_owned(),
        Value::Array(
            config
                .disabled_tools
                .iter()
                .cloned()
                .map(Value::String)
                .collect(),
        ),
    );
    table.insert(
        "startup_timeout_sec".to_owned(),
        Value::Float(config.startup_timeout_ms as f64 / 1_000.0),
    );
    table.insert(
        "tool_timeout_sec".to_owned(),
        Value::Float(config.tool_timeout_ms as f64 / 1_000.0),
    );
    Ok(table)
}

pub(super) fn insert_http_mcp_fields(
    table: &mut Table,
    url: &Url,
    headers: &BTreeMap<String, String>,
) {
    table.insert("url".to_owned(), Value::String(url.to_string()));
    if !headers.is_empty() {
        table.insert(
            "headers".to_owned(),
            Value::Table(
                headers
                    .iter()
                    .map(|(key, value)| (key.clone(), Value::String(value.clone())))
                    .collect(),
            ),
        );
    }
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

    pub(super) fn invalid(self, message: &str) -> ConfigError {
        match self {
            Self::McpServers => ConfigError::InvalidMcp(message.to_owned()),
            Self::Connectors => ConfigError::InvalidIntegration(message.to_owned()),
        }
    }
}
