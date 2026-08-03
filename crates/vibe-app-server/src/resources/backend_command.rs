use std::collections::BTreeMap;
use std::path::PathBuf;

use serde_json::Value;
use url::Url;

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
    pub alias: String,
    pub transport: McpAddTransport,
    pub enabled: bool,
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
    if params.contains_key("scopes") || params.contains_key("login") {
        return Err(ResourceError::InvalidParams(
            "this runtime does not support implicit OAuth scopes or login during MCP add"
                .to_owned(),
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
        "streamable-http" => &["sessionId", "transport", "name", "disabled", "url"],
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
    let (default_alias, transport) = match transport {
        "stdio" => {
            let command = required_string(params, "command")?.to_owned();
            let alias = mcp_command_alias(&command);
            (
                alias,
                McpAddTransport::Stdio {
                    command,
                    arguments: optional_string_list(params, "arguments")?.unwrap_or_default(),
                    environment: optional_string_map(params, "environment")?.unwrap_or_default(),
                    working_directory: optional_string(params, "workingDirectory")?
                        .map(PathBuf::from),
                },
            )
        }
        "streamable-http" => {
            let url = Url::parse(required_string(params, "url")?)
                .map_err(|_| ResourceError::InvalidParams("url must be valid HTTPS".to_owned()))?;
            if url.scheme() != "https" {
                return Err(ResourceError::InvalidParams(
                    "MCP HTTP endpoints require HTTPS".to_owned(),
                ));
            }
            let alias = mcp_alias(&url);
            (alias, McpAddTransport::Http { url })
        }
        _ => unreachable!("transport was validated above"),
    };
    let alias = optional_string(params, "name")?
        .unwrap_or(&default_alias)
        .to_owned();
    if alias.is_empty() {
        return Err(ResourceError::InvalidParams(
            "MCP source name cannot be empty".to_owned(),
        ));
    }
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
        alias,
        transport,
        enabled,
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

fn mcp_alias(url: &Url) -> String {
    let alias = url
        .host_str()
        .unwrap_or("mcp")
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect::<String>();
    alias.trim_matches('_').to_owned()
}

fn mcp_command_alias(command: &str) -> String {
    PathBuf::from(command)
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or("mcp")
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect::<String>()
        .trim_matches('_')
        .to_owned()
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
                alias: "example".to_owned(),
                transport: McpAddTransport::Http {
                    url: Url::parse("https://mcp.example/tools").expect("URL fixture"),
                },
                enabled: true,
            }))
        );
    }

    #[test]
    fn rejects_insecure_mcp_url_before_dispatch() {
        let params = BTreeMap::from([("url".to_owned(), json!("http://mcp.example"))]);

        assert!(matches!(
            ResourceBackendCommand::parse("mcp/add", &params, false),
            Err(ResourceError::InvalidParams(message))
                if message == "MCP HTTP endpoints require HTTPS"
        ));
    }

    #[test]
    fn rejects_legacy_sse_transport_instead_of_misrouting_it_as_streamable_http() {
        for transport in ["http", "sse"] {
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

    #[test]
    fn rejects_unimplemented_mcp_oauth_shortcuts_instead_of_dropping_them() {
        for key in ["scopes", "login"] {
            let mut params =
                BTreeMap::from([("url".to_owned(), json!("https://mcp.example/tools"))]);
            params.insert(
                key.to_owned(),
                if key == "scopes" {
                    json!(["read"])
                } else {
                    json!(false)
                },
            );

            assert!(matches!(
                ResourceBackendCommand::parse("mcp/add", &params, false),
                Err(ResourceError::InvalidParams(message))
                    if message.contains("does not support implicit OAuth")
            ));
        }
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
