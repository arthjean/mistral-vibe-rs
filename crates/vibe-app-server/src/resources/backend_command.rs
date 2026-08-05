use std::collections::BTreeMap;
use std::path::PathBuf;

use serde_json::Value;
use url::Url;
use vibe_core::config::mcp::{normalize_mcp_server_name, normalize_mcp_server_url};

use super::ResourceError;

use crate::params::MAX_PARAM_STRING_BYTES as MAX_STRING_BYTES;
const MAX_COLLECTION_ENTRIES: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResourceBackendCommand {
    Connector(ConnectorCommand),
    Mcp(McpCommand),
    Shell(ShellCommand),
}

impl ResourceBackendCommand {
    pub fn parse(
        method: &str,
        params: &BTreeMap<String, Value>,
        session_active: bool,
    ) -> Result<Self, ResourceError> {
        let command = match method {
            "connectors/read" => Self::Connector(ConnectorCommand::Read),
            "connectors/auth/read" => Self::Connector(ConnectorCommand::AuthRead {
                name: required_string(params, "name")?.to_owned(),
            }),
            "connectors/refresh" => Self::Connector(ConnectorCommand::Refresh {
                name: required_string(params, "name")?.to_owned(),
            }),
            "connectors/toggle" => Self::Connector(ConnectorCommand::Toggle {
                name: required_string(params, "name")?.to_owned(),
                disabled: required_bool(params, "disabled")?,
                tool_name: optional_string(params, "toolName")?.map(str::to_owned),
            }),
            "mcp/read" => Self::Mcp(McpCommand::Read),
            "mcp/add" => Self::Mcp(McpCommand::Add(parse_mcp_add(params)?)),
            "mcp/refresh" => Self::Mcp(McpCommand::Refresh {
                name: required_string(params, "name")?.to_owned(),
            }),
            "mcp/toggle" => Self::Mcp(McpCommand::Toggle {
                name: required_string(params, "name")?.to_owned(),
                disabled: required_bool(params, "disabled")?,
                tool_name: optional_string(params, "toolName")?.map(str::to_owned),
            }),
            "mcp/login" => Self::Mcp(McpCommand::Login {
                name: required_string(params, "name")?.to_owned(),
            }),
            "mcp/auth/complete" => Self::Mcp(McpCommand::CompleteAuth {
                name: required_string(params, "name")?.to_owned(),
            }),
            "mcp/logout" => Self::Mcp(McpCommand::Logout {
                name: required_string(params, "name")?.to_owned(),
            }),
            "shell/run" => {
                if session_active {
                    return Err(ResourceError::Conflict(
                        "manual shell cannot run during an active turn".to_owned(),
                    ));
                }
                let command = required_string(params, "command")?.to_owned();
                if command.trim().is_empty() {
                    return Err(ResourceError::InvalidParams(
                        "shell command cannot be empty".to_owned(),
                    ));
                }
                Self::Shell(ShellCommand::Run {
                    operation_id: required_string(params, "operationId")?.to_owned(),
                    command,
                })
            }
            "shell/interrupt" => Self::Shell(ShellCommand::Interrupt {
                operation_id: required_string(params, "operationId")?.to_owned(),
            }),
            _ => return Err(ResourceError::MethodNotFound(method.to_owned())),
        };
        Ok(command)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectorCommand {
    Read,
    AuthRead {
        name: String,
    },
    Refresh {
        name: String,
    },
    Toggle {
        name: String,
        disabled: bool,
        tool_name: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpCommand {
    Read,
    Add(McpAddCommand),
    Refresh {
        name: String,
    },
    Toggle {
        name: String,
        disabled: bool,
        tool_name: Option<String>,
    },
    Login {
        name: String,
    },
    CompleteAuth {
        name: String,
    },
    Logout {
        name: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpAddCommand {
    /// The alias the caller asked for, already normalized. `None` leaves the
    /// alias to be derived from the URL and deduplicated against the servers
    /// this session already knows, which needs the session's own state.
    pub requested_alias: Option<String>,
    pub transport: McpAddTransport,
    pub enabled: bool,
    /// OAuth scopes to persist with the entry, as the reference persists what
    /// the caller asked for.
    pub scopes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpAddTransport {
    Stdio {
        command: String,
        arguments: Vec<String>,
        environment: BTreeMap<String, String>,
        working_directory: Option<PathBuf>,
    },
    Http {
        url: Url,
        /// Whether the entry was asked for under `http` rather than
        /// `streamable-http`, which decides the name it is persisted with.
        legacy: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellCommand {
    Run {
        operation_id: String,
        command: String,
    },
    Interrupt {
        operation_id: String,
    },
}

fn parse_mcp_add(params: &BTreeMap<String, Value>) -> Result<McpAddCommand, ResourceError> {
    if params.contains_key("login") {
        return Err(ResourceError::InvalidParams(
            "this runtime does not support implicit login during MCP add".to_owned(),
        ));
    }
    let transport = optional_string(params, "transport")?.unwrap_or("streamable-http");
    let allowed_parameters: &[&str] = match transport {
        "stdio" => &[
            "sessionId",
            "transport",
            "name",
            "disabled",
            "command",
            "arguments",
            "environment",
            "workingDirectory",
        ],
        "http" | "streamable-http" => &[
            "sessionId",
            "transport",
            "name",
            "disabled",
            "url",
            "scopes",
        ],
        _ => {
            return Err(ResourceError::InvalidParams(
                "unsupported MCP transport".to_owned(),
            ));
        }
    };
    if let Some(parameter) = params
        .keys()
        .find(|parameter| !allowed_parameters.contains(&parameter.as_str()))
    {
        return Err(ResourceError::InvalidParams(format!(
            "unsupported MCP add parameter `{parameter}` for {transport} transport"
        )));
    }
    let transport = match transport {
        "stdio" => McpAddTransport::Stdio {
            command: required_string(params, "command")?.to_owned(),
            arguments: optional_string_list(params, "arguments")?.unwrap_or_default(),
            environment: optional_string_map(params, "environment")?.unwrap_or_default(),
            working_directory: optional_string(params, "workingDirectory")?.map(PathBuf::from),
        },
        legacy_or_streamable => {
            // Every rejection an MCP URL can earn lives in the store, so the
            // same spelling is refused here and by a file written by hand.
            let normalized =
                normalize_mcp_server_url(required_string(params, "url")?).map_err(|error| {
                    ResourceError::InvalidParams(match error {
                        vibe_core::config::ConfigError::InvalidMcp(message) => message,
                        error => error.to_string(),
                    })
                })?;
            let url = Url::parse(&normalized).map_err(|_| {
                ResourceError::InvalidParams("url must be a valid HTTP(S) URL".to_owned())
            })?;
            McpAddTransport::Http {
                url,
                legacy: legacy_or_streamable == "http",
            }
        }
    };
    let requested_alias = match optional_string(params, "name")? {
        None => None,
        Some(name) => {
            let normalized = normalize_mcp_server_name(name);
            if normalized.is_empty() {
                return Err(ResourceError::InvalidParams(
                    "MCP server name must contain letters or numbers".to_owned(),
                ));
            }
            Some(normalized)
        }
    };
    let scopes = optional_string_list(params, "scopes")?.unwrap_or_default();
    let enabled = match params.get("disabled") {
        None | Some(Value::Null) => true,
        Some(Value::Bool(disabled)) => !disabled,
        Some(_) => {
            return Err(ResourceError::InvalidParams(
                "disabled must be a boolean".to_owned(),
            ));
        }
    };
    Ok(McpAddCommand {
        requested_alias,
        transport,
        enabled,
        scopes,
    })
}

fn invalid_params(error: crate::params::ParamError) -> ResourceError {
    ResourceError::InvalidParams(error.message())
}

fn required_string<'a>(
    values: &'a BTreeMap<String, Value>,
    key: &str,
) -> Result<&'a str, ResourceError> {
    crate::params::required_string(values, key).map_err(invalid_params)
}

fn optional_string<'a>(
    values: &'a BTreeMap<String, Value>,
    key: &str,
) -> Result<Option<&'a str>, ResourceError> {
    crate::params::optional_string(values, key).map_err(invalid_params)
}

fn required_bool(values: &BTreeMap<String, Value>, key: &str) -> Result<bool, ResourceError> {
    crate::params::required_bool(values, key).map_err(invalid_params)
}

fn optional_string_list(
    params: &BTreeMap<String, Value>,
    key: &str,
) -> Result<Option<Vec<String>>, ResourceError> {
    let Some(value) = params.get(key) else {
        return Ok(None);
    };
    let values = value
        .as_array()
        .ok_or_else(|| ResourceError::InvalidParams(format!("{key} must be an array")))?;
    if values.len() > MAX_COLLECTION_ENTRIES {
        return Err(ResourceError::InvalidParams(format!(
            "{key} contains too many entries"
        )));
    }
    values
        .iter()
        .map(|value| {
            value
                .as_str()
                .filter(|value| value.len() <= MAX_STRING_BYTES && !value.contains('\0'))
                .map(str::to_owned)
                .ok_or_else(|| {
                    ResourceError::InvalidParams(format!("{key} entries must be bounded strings"))
                })
        })
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
}

fn optional_string_map(
    params: &BTreeMap<String, Value>,
    key: &str,
) -> Result<Option<BTreeMap<String, String>>, ResourceError> {
    let Some(value) = params.get(key) else {
        return Ok(None);
    };
    let values = value
        .as_object()
        .ok_or_else(|| ResourceError::InvalidParams(format!("{key} must be an object")))?;
    if values.len() > MAX_COLLECTION_ENTRIES {
        return Err(ResourceError::InvalidParams(format!(
            "{key} contains too many entries"
        )));
    }
    values
        .iter()
        .map(|(name, value)| {
            let value = value
                .as_str()
                .filter(|value| value.len() <= MAX_STRING_BYTES && !value.contains('\0'))
                .ok_or_else(|| {
                    ResourceError::InvalidParams(format!("{key} values must be bounded strings"))
                })?;
            Ok((name.clone(), value.to_owned()))
        })
        .collect::<Result<BTreeMap<_, _>, _>>()
        .map(Some)
}

/// The alias a stdio server is suggested, which the reference derives from the
/// executable because a command has no host to name it after.
pub fn mcp_command_alias(command: &str) -> String {
    let stem = PathBuf::from(command)
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or("mcp")
        .to_lowercase();
    let normalized = normalize_mcp_server_name(&stem);
    if normalized.is_empty() {
        "mcp".to_owned()
    } else {
        normalized
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn parses_mcp_add_into_a_validated_transport() {
        let params = json!({
            "url": "https://mcp.example/tools",
            "name": "example"
        })
        .as_object()
        .expect("params object")
        .clone()
        .into_iter()
        .collect();

        assert_eq!(
            ResourceBackendCommand::parse("mcp/add", &params, false).expect("validated command"),
            ResourceBackendCommand::Mcp(McpCommand::Add(McpAddCommand {
                requested_alias: Some("example".to_owned()),
                transport: McpAddTransport::Http {
                    url: Url::parse("https://mcp.example/tools").expect("URL fixture"),
                    legacy: false,
                },
                enabled: true,
                scopes: Vec::new(),
            }))
        );
    }

    #[test]
    fn rejects_insecure_mcp_url_before_dispatch() {
        let params = BTreeMap::from([("url".to_owned(), json!("http://mcp.example"))]);

        assert!(matches!(
            ResourceBackendCommand::parse("mcp/add", &params, false),
            Err(ResourceError::InvalidParams(message))
                if message == "MCP server URL must use https unless it points to localhost"
        ));
    }

    /// The reference publishes `http` beside `streamable-http`, and the two
    /// speak the same exchange, so the transport only decides the name the
    /// entry is persisted under.
    #[test]
    fn accepts_the_legacy_http_transport_under_its_own_name() {
        let params = json!({
            "transport": "http",
            "url": "https://mcp.example/tools",
        })
        .as_object()
        .expect("params object")
        .clone()
        .into_iter()
        .collect();

        assert_eq!(
            ResourceBackendCommand::parse("mcp/add", &params, false).expect("validated command"),
            ResourceBackendCommand::Mcp(McpCommand::Add(McpAddCommand {
                requested_alias: None,
                transport: McpAddTransport::Http {
                    url: Url::parse("https://mcp.example/tools").expect("URL fixture"),
                    legacy: true,
                },
                enabled: true,
                scopes: Vec::new(),
            }))
        );
    }

    #[test]
    fn rejects_an_unknown_transport_instead_of_misrouting_it_as_streamable_http() {
        for transport in ["sse", "websocket"] {
            let params = json!({
                "transport": transport,
                "url": "https://mcp.example/tools",
            })
            .as_object()
            .expect("params object")
            .clone()
            .into_iter()
            .collect();
            assert!(matches!(
                ResourceBackendCommand::parse("mcp/add", &params, false),
                Err(ResourceError::InvalidParams(message))
                    if message == "unsupported MCP transport"
            ));
        }
    }

    /// Scopes are persisted with the entry, as the reference persists them;
    /// only the login shortcut stays unimplemented here.
    #[test]
    fn keeps_requested_oauth_scopes_and_rejects_the_login_shortcut() {
        let mut params = BTreeMap::from([("url".to_owned(), json!("https://mcp.example/tools"))]);
        params.insert("scopes".to_owned(), json!(["repo", "read"]));

        let scopes = match ResourceBackendCommand::parse("mcp/add", &params, false)
            .expect("validated command")
        {
            ResourceBackendCommand::Mcp(McpCommand::Add(add)) => add.scopes,
            other => unreachable!("mcp/add parses into an add command, got {other:?}"),
        };
        assert_eq!(scopes, ["repo".to_owned(), "read".to_owned()]);

        params.remove("scopes");
        params.insert("login".to_owned(), json!(false));
        assert!(matches!(
            ResourceBackendCommand::parse("mcp/add", &params, false),
            Err(ResourceError::InvalidParams(message))
                if message.contains("does not support implicit login")
        ));
    }

    #[test]
    fn rejects_transport_parameters_that_would_be_silently_ignored() {
        for params in [
            json!({"url": "https://mcp.example/tools", "headers": {"x-key": "secret"}}),
            json!({"url": "https://mcp.example/tools", "command": "ignored"}),
            json!({"transport": "stdio", "command": "server", "url": "https://ignored"}),
        ] {
            let params = params
                .as_object()
                .expect("params object")
                .clone()
                .into_iter()
                .collect();
            assert!(matches!(
                ResourceBackendCommand::parse("mcp/add", &params, false),
                Err(ResourceError::InvalidParams(message))
                    if message.contains("unsupported MCP add parameter")
            ));
        }
    }

    #[test]
    fn rejects_empty_domain_identifiers() {
        for (method, params) in [
            (
                "mcp/refresh",
                BTreeMap::from([("name".to_owned(), json!("  "))]),
            ),
            (
                "shell/interrupt",
                BTreeMap::from([("operationId".to_owned(), json!(""))]),
            ),
        ] {
            assert!(matches!(
                ResourceBackendCommand::parse(method, &params, false),
                Err(ResourceError::InvalidParams(_))
            ));
        }
    }
}
