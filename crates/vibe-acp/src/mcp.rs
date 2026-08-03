//! Projection of ACP MCP server declarations onto the public app-server shape.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use crate::protocol::AcpError;

const MAX_MCP_SERVERS: usize = 64;

pub(crate) fn project_acp_mcp_servers(servers: &[Value]) -> Result<Vec<Value>, AcpError> {
    if servers.len() > MAX_MCP_SERVERS {
        return Err(AcpError::InvalidParams(format!(
            "mcpServers cannot contain more than {MAX_MCP_SERVERS} servers"
        )));
    }
    servers.iter().map(project_server).collect()
}

fn project_server(server: &Value) -> Result<Value, AcpError> {
    let object = server
        .as_object()
        .ok_or_else(|| AcpError::InvalidParams("MCP server must be an object".to_owned()))?;
    let transport = object
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| AcpError::InvalidParams("MCP server requires a type".to_owned()))?;
    let name = required_field(object.get("name"), "stdio MCP server requires a name")?;
    match transport {
        "stdio" => Ok(json!({
            "name": name,
            "transport": "stdio",
            "command": required_field(
                object.get("command"),
                "stdio MCP server requires a command",
            )?,
            "args": string_array(object.get("args"), "stdio MCP server args must be strings")?,
            "env": named_values(object.get("env"), "stdio MCP environment entry")?,
        })),
        "http" => Ok(json!({
            "name": name,
            "transport": "streamable-http",
            "url": required_field(object.get("url"), "HTTP MCP server requires a URL")?,
            "headers": named_values(object.get("headers"), "HTTP MCP header")?,
        })),
        unsupported => Err(AcpError::UnsupportedClientFlow(format!(
            "MCP transport `{unsupported}` is not supported by the public app-server"
        ))),
    }
}

fn required_field<'a>(value: Option<&'a Value>, message: &str) -> Result<&'a str, AcpError> {
    value
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| AcpError::InvalidParams(message.to_owned()))
}

fn string_array(value: Option<&Value>, message: &str) -> Result<Vec<String>, AcpError> {
    entries(value)
        .map(|entry| {
            entry
                .as_str()
                .map(ToOwned::to_owned)
                .ok_or_else(|| AcpError::InvalidParams(message.to_owned()))
        })
        .collect()
}

/// `[{"name": ..., "value": ...}]` pairs, which both stdio environments and
/// HTTP headers use.
fn named_values(value: Option<&Value>, label: &str) -> Result<BTreeMap<String, String>, AcpError> {
    entries(value)
        .map(|entry| {
            let name = required_field(entry.get("name"), &format!("{label} requires a name"))?;
            let value = entry
                .get("value")
                .and_then(Value::as_str)
                .ok_or_else(|| AcpError::InvalidParams(format!("{label} requires a value")))?;
            Ok((name.to_owned(), value.to_owned()))
        })
        .collect()
}

fn entries(value: Option<&Value>) -> impl Iterator<Item = &Value> {
    value
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
}
