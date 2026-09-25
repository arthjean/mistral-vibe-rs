use std::collections::BTreeMap;

use serde_json::Value;

use super::ResourceError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResourceBackendCommand {
    Connector(ConnectorCommand),
    Shell(ShellCommand),
}

impl ResourceBackendCommand {
    /// The method this command was parsed from.
    ///
    /// The deferred path carries the command rather than the request, and the
    /// answer's shape is decided by the method, so the name travels with it.
    #[must_use]
    pub const fn method(&self) -> &'static str {
        match self {
            Self::Connector(ConnectorCommand::Read) => "connectors/read",
            Self::Connector(ConnectorCommand::AuthRead { .. }) => "connectors/auth/read",
            Self::Connector(ConnectorCommand::Refresh { .. }) => "connectors/refresh",
            Self::Connector(ConnectorCommand::Toggle { .. }) => "connectors/toggle",
            Self::Shell(ShellCommand::Run { .. }) => "shell/run",
            Self::Shell(ShellCommand::Interrupt { .. }) => "shell/interrupt",
        }
    }

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
pub enum ShellCommand {
    Run {
        operation_id: String,
        command: String,
    },
    Interrupt {
        operation_id: String,
    },
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn rejects_empty_domain_identifiers() {
        for (method, params) in [
            (
                "connectors/refresh",
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
