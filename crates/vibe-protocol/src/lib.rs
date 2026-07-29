#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use std::collections::BTreeMap;

use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;

pub const PROTOCOL_VERSION: &str = "1";

pub const SERVER_METHODS: [&str; 79] = [
    "account/read",
    "agents/install",
    "agents/list",
    "agents/uninstall",
    "callback/respond",
    "config/batchWrite",
    "config/proxy/read",
    "config/proxy/write",
    "config/read",
    "config/reload",
    "config/schema",
    "config/thinking/write",
    "connectors/auth/read",
    "connectors/read",
    "connectors/refresh",
    "diagnostics/list",
    "diagnostics/logs/read",
    "feedback/record",
    "feedback/shouldShow",
    "history/list",
    "loops/clear",
    "loops/create",
    "loops/delete",
    "loops/list",
    "mcp/add",
    "mcp/login",
    "mcp/logout",
    "mcp/read",
    "mcp/refresh",
    "mcp/toggle",
    "narration/summarize",
    "review/approve",
    "review/baseline",
    "review/hunks",
    "review/revert",
    "review/state",
    "review/turnDiff",
    "runtime/read",
    "session/agent/update",
    "session/close",
    "session/compact/start",
    "session/continue",
    "session/context/inject",
    "session/delete",
    "session/fork",
    "session/history/clear",
    "session/list",
    "session/log/read",
    "session/read",
    "session/ready/read",
    "session/ready/wait",
    "session/resume",
    "session/rewind",
    "session/rewind/read",
    "session/settings/update",
    "session/start",
    "session/title/update",
    "shell/interrupt",
    "shell/run",
    "skills/list",
    "stats/read",
    "telemetry/record",
    "tools/list",
    "turn/interrupt",
    "turn/start",
    "turn/steer",
    "vibeCode/projects/cancel",
    "vibeCode/projects/create",
    "vibeCode/projects/loadMore",
    "vibeCode/projects/open",
    "vibeCode/projects/recover",
    "vibeCode/projects/select",
    "vibeCode/projects/unlink",
    "vibeCode/teleport/cancel",
    "vibeCode/teleport/push/respond",
    "vibeCode/teleport/start",
    "workspace/prompt/prepare",
    "workspace/trust/decision",
    "workspace/trust/status",
];

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum RequestId {
    Integer(i64),
    String(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProtocolErrorCode {
    InvalidRequest,
    InvalidParams,
    NotInitialized,
    NotFound,
    Conflict,
    StaleTurn,
    NotSteerable,
    CompactionFailed,
    Unauthorized,
    Forbidden,
    MethodNotFound,
    InternalError,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProtocolError {
    pub code: ProtocolErrorCode,
    pub message: String,
    #[serde(default)]
    pub data: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Notification {
    pub jsonrpc: JsonRpcVersion,
    pub method: String,
    pub params: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ServerRequest {
    pub jsonrpc: JsonRpcVersion,
    pub id: RequestId,
    pub method: String,
    pub params: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SuccessResponse {
    pub jsonrpc: JsonRpcVersion,
    pub id: RequestId,
    pub result: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ErrorResponse {
    pub jsonrpc: JsonRpcVersion,
    pub id: RequestId,
    pub error: ProtocolError,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum JsonRpcVersion {
    #[default]
    #[serde(rename = "2.0")]
    V2,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum Envelope {
    Notification(Notification),
    Request(ServerRequest),
    Success(SuccessResponse),
    Error(ErrorResponse),
}

#[derive(Debug, Error)]
pub enum ProtocolValidationError {
    #[error("invalid JSON frame: {0}")]
    Json(#[from] serde_json::Error),
    #[error("unknown server method: {0}")]
    UnknownMethod(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum ConnectionOutcome {
    Frame(Envelope),
    Close {
        code: ProtocolErrorCode,
        message: &'static str,
    },
}

pub fn decode_frame(bytes: &[u8]) -> Result<Envelope, ProtocolValidationError> {
    Ok(serde_json::from_slice(bytes)?)
}

#[must_use]
pub fn validate_connection_frame(bytes: &[u8]) -> ConnectionOutcome {
    match decode_frame(bytes) {
        Ok(frame) => ConnectionOutcome::Frame(frame),
        Err(ProtocolValidationError::Json(_)) => ConnectionOutcome::Close {
            code: ProtocolErrorCode::InvalidRequest,
            message: "Malformed JSON-RPC message",
        },
        Err(ProtocolValidationError::UnknownMethod(_)) => ConnectionOutcome::Close {
            code: ProtocolErrorCode::MethodNotFound,
            message: "Unknown app-server method",
        },
    }
}

pub fn encode_frame(frame: &Envelope) -> Result<Vec<u8>, ProtocolValidationError> {
    Ok(serde_json::to_vec(frame)?)
}

pub fn validate_server_method(method: &str) -> Result<(), ProtocolValidationError> {
    if SERVER_METHODS.contains(&method) {
        Ok(())
    } else {
        Err(ProtocolValidationError::UnknownMethod(method.to_owned()))
    }
}

pub fn protocol_schema() -> Value {
    serde_json::json!({
        "protocolVersion": PROTOCOL_VERSION,
        "serverMethods": &SERVER_METHODS[..],
        "envelope": schema_for!(Envelope),
        "initializeParams": schema_for!(InitializeParams),
        "initializeResponse": schema_for!(InitializeResponse),
        "sessionMcpServer": schema_for!(SessionMcpServer),
        "protocolError": schema_for!(ProtocolError),
    })
}

pub fn protocol_schema_digest() -> String {
    let encoded = serde_json::to_vec(&protocol_schema()).unwrap_or_default();
    format!("sha256:{}", hex_digest(Sha256::digest(encoded)))
}

fn hex_digest(bytes: impl AsRef<[u8]>) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let bytes = bytes.as_ref();
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ClientEntrypoint {
    #[default]
    Unknown,
    Cli,
    Acp,
    Programmatic,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TerminalEmulator {
    Vscode,
    VscodeInsiders,
    Cursor,
    Jetbrains,
    AppleTerminal,
    Iterm2,
    Wezterm,
    Ghostty,
    Alacritty,
    Kitty,
    Hyper,
    WindowsTerminal,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ClientInfo {
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub entrypoint: ClientEntrypoint,
    #[serde(default)]
    pub terminal_emulator: TerminalEmulator,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CallbackKind {
    Approval,
    UserInput,
    ConnectorAuth,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum ClientToolCapability {
    #[serde(rename = "filesystem/read")]
    FilesystemRead,
    #[serde(rename = "filesystem/write")]
    FilesystemWrite,
    #[serde(rename = "terminal")]
    Terminal,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ClientCapabilities {
    #[serde(default)]
    pub callback_kinds: Vec<CallbackKind>,
    #[serde(default)]
    pub client_tools: Vec<ClientToolCapability>,
    #[serde(default)]
    pub disabled_notifications: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct InitializeParams {
    pub client_info: ClientInfo,
    #[serde(default)]
    pub capabilities: ClientCapabilities,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ServerInfo {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum TransportKind {
    #[serde(rename = "in_process")]
    InProcess,
    #[serde(rename = "stdio")]
    Stdio,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ServerCapabilities {
    #[serde(default)]
    pub methods: Vec<String>,
    #[serde(default)]
    pub callback_kinds: Vec<CallbackKind>,
    #[serde(default)]
    pub transports: Vec<TransportKind>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct InitializeResponse {
    pub server_info: ServerInfo,
    pub protocol_version: ProtocolVersion,
    pub capabilities: ServerCapabilities,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum ProtocolVersion {
    #[default]
    #[serde(rename = "1")]
    V1,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "transport", rename_all = "kebab-case")]
pub enum SessionMcpServer {
    Http {
        name: String,
        url: String,
        #[serde(default)]
        headers: BTreeMap<String, String>,
    },
    StreamableHttp {
        name: String,
        url: String,
        #[serde(default)]
        headers: BTreeMap<String, String>,
    },
    Stdio {
        name: String,
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: BTreeMap<String, String>,
        cwd: Option<String>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn valid_frames_round_trip_canonically() {
        let input = br#"{"jsonrpc":"2.0","id":"client-1","result":{"ok":true}}"#;
        let first = decode_frame(input).expect("valid fixture");
        let encoded = encode_frame(&first).expect("serializable fixture");
        let second = decode_frame(&encoded).expect("round-trip fixture");
        assert_eq!(first, second);
        assert_eq!(
            encoded,
            br#"{"jsonrpc":"2.0","id":"client-1","result":{"ok":true}}"#
        );
    }

    #[test]
    fn malformed_envelopes_are_rejected() {
        for value in [
            json!({"jsonrpc": "2.0", "id": "client-1"}),
            json!({"jsonrpc": "2.0", "id": "client-1", "result": {}, "error": {
                "code": "internal_error", "message": "failed"
            }}),
            json!({"jsonrpc": "2.0", "method": "initialized", "params": {}, "extra": true}),
            json!({"jsonrpc": "2.0", "id": true, "result": {}}),
            json!({"jsonrpc": "2.0", "id": 1, "result": []}),
        ] {
            let encoded = serde_json::to_vec(&value).expect("JSON fixture");
            assert!(decode_frame(&encoded).is_err(), "accepted {value}");
            assert!(matches!(
                validate_connection_frame(&encoded),
                ConnectionOutcome::Close {
                    code: ProtocolErrorCode::InvalidRequest,
                    message: "Malformed JSON-RPC message"
                }
            ));
        }
    }

    #[test]
    fn camel_case_is_strict_and_variants_are_closed() {
        let snake = json!({
            "client_info": {
                "name": "test",
                "version": "1",
                "title": null,
                "entrypoint": "unknown",
                "terminal_emulator": "unknown"
            }
        });
        assert!(serde_json::from_value::<InitializeParams>(snake).is_err());
        assert!(serde_json::from_value::<CallbackKind>(json!("unknown")).is_err());
    }

    #[test]
    fn method_inventory_is_complete_and_unique() {
        let unique = SERVER_METHODS
            .iter()
            .copied()
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(unique.len(), 79);
        assert_eq!(unique.len(), SERVER_METHODS.len());
    }
}
