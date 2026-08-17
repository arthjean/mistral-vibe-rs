//! The JSON-RPC frames this transport reads and writes.

use serde::Deserialize;
use serde_json::{Value, json};
use vibe_acp::AcpError;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WireRequest {
    pub(crate) jsonrpc: String,
    #[serde(default)]
    pub(crate) id: Value,
    pub(crate) method: String,
    #[serde(default)]
    pub(crate) params: Value,
}

pub(crate) fn success_response(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

pub(crate) fn error_response(id: Value, code: i64, message: String) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {"code": code, "message": message},
    })
}

pub(crate) fn acp_error_response(id: Value, error: AcpError) -> Value {
    error_response(id, error.json_rpc_code(), error.to_string())
}

pub(crate) fn valid_id(id: &Value) -> bool {
    id.is_i64() || id.is_u64() || id.is_string()
}

pub(crate) fn required_string<'a>(params: &'a Value, key: &str) -> Result<&'a str, AcpError> {
    params
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| AcpError::InvalidParams(format!("{key} must be a non-empty string")))
}

pub(crate) fn scalar_string(value: &Value) -> Result<String, AcpError> {
    match value {
        Value::String(value) => Ok(value.clone()),
        Value::Bool(value) => Ok(value.to_string()),
        Value::Number(value) => Ok(value.to_string()),
        _ => Err(AcpError::InvalidParams(
            "config value must be a string, boolean, or number".to_owned(),
        )),
    }
}
