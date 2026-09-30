use std::collections::BTreeMap;

use serde_json::Value;

use super::ResourceError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResourceBackendCommand {
    Shell(ShellCommand),
}

impl ResourceBackendCommand {
    pub fn parse(
        method: &str,
        params: &BTreeMap<String, Value>,
        session_active: bool,
    ) -> Result<Self, ResourceError> {
        let command = match method {
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn rejects_empty_domain_identifiers() {
        let params = BTreeMap::from([("operationId".to_owned(), json!(""))]);
        assert!(matches!(
            ResourceBackendCommand::parse("shell/interrupt", &params, false),
            Err(ResourceError::InvalidParams(_))
        ));
    }
}
