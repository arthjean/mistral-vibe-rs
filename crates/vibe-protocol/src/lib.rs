#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use std::collections::BTreeMap;

use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

pub const PROTOCOL_VERSION: &str = "1";

/// Every method the app-server routes, sorted and unique.
///
/// Lifecycle methods (`initialize`, `initialized`, `shutdown`, `exit`) are
/// deliberately absent: they are handled before method dispatch and are not
/// part of the negotiated surface.
pub const SERVER_METHODS: [&str; 80] = [
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
    "connectors/toggle",
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
    "mcp/auth/complete",
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
    "session/context/inject",
    "session/continue",
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
    /// Optional structured detail; omitted from the wire when null.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub data: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Notification {
    pub jsonrpc: JsonRpcVersion,
    pub method: String,
    /// Payload; absent on the wire is equivalent to empty, per JSON-RPC 2.0.
    #[serde(default)]
    pub params: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ServerRequest {
    pub jsonrpc: JsonRpcVersion,
    pub id: RequestId,
    pub method: String,
    /// Payload; absent on the wire is equivalent to empty, per JSON-RPC 2.0.
    #[serde(default)]
    pub params: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SuccessResponse {
    pub jsonrpc: JsonRpcVersion,
    pub id: RequestId,
    /// Method-specific payload. Required: a response carries `result` or
    /// `error`, never neither.
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
}

/// Decodes an inbound frame.
///
/// A frame that fails here carries no usable `id`, so the protocol has no way
/// to answer it: callers close the connection instead of replying.
pub fn decode_frame(bytes: &[u8]) -> Result<Envelope, ProtocolValidationError> {
    Ok(serde_json::from_slice(bytes)?)
}

/// Serializes an outbound frame.
///
/// Every field of [`Envelope`] is JSON-native (strings, `i64`,
/// [`serde_json::Value`], unit enums and maps keyed by `String`), so
/// serialization has no failure mode and callers get bytes, not a `Result`
/// they would have to discard.
#[must_use]
#[expect(
    clippy::expect_used,
    reason = "Envelope is JSON-native by construction; serialization cannot fail"
)]
pub fn encode_frame(frame: &Envelope) -> Vec<u8> {
    serde_json::to_vec(frame).expect("Envelope serialization is infallible")
}

/// Reports whether `method` is part of the negotiated surface.
#[must_use]
pub fn is_server_method(method: &str) -> bool {
    SERVER_METHODS.binary_search(&method).is_ok()
}

/// JSON Schema description of the wire contract, for external client
/// generators.
#[must_use]
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
        let encoded = encode_frame(&first);
        let second = decode_frame(&encoded).expect("round-trip fixture");
        assert_eq!(first, second);
        assert_eq!(
            encoded,
            br#"{"jsonrpc":"2.0","id":"client-1","result":{"ok":true}}"#
        );
    }

    #[test]
    fn each_envelope_variant_is_decoded_unambiguously() {
        let cases = [
            (
                json!({"jsonrpc": "2.0", "method": "turn/started", "params": {}}),
                "notification",
            ),
            (
                json!({"jsonrpc": "2.0", "method": "turn/started"}),
                "notification",
            ),
            (
                json!({"jsonrpc": "2.0", "id": 1, "method": "turn/start", "params": {}}),
                "request",
            ),
            (
                json!({"jsonrpc": "2.0", "id": 1, "method": "turn/start"}),
                "request",
            ),
            (json!({"jsonrpc": "2.0", "id": 1, "result": {}}), "success"),
            (
                json!({"jsonrpc": "2.0", "id": 1, "error": {"code": "not_found", "message": "gone"}}),
                "error",
            ),
        ];
        for (value, expected) in cases {
            let encoded = serde_json::to_vec(&value).expect("JSON fixture");
            let decoded = decode_frame(&encoded).expect("valid frame");
            let actual = match decoded {
                Envelope::Notification(_) => "notification",
                Envelope::Request(_) => "request",
                Envelope::Success(_) => "success",
                Envelope::Error(_) => "error",
            };
            assert_eq!(actual, expected, "misrouted {value}");
        }
    }

    #[test]
    fn null_error_data_stays_off_the_wire() {
        let frame = Envelope::Error(ErrorResponse {
            jsonrpc: JsonRpcVersion::V2,
            id: RequestId::Integer(1),
            error: ProtocolError {
                code: ProtocolErrorCode::NotFound,
                message: "gone".to_owned(),
                data: Value::Null,
            },
        });
        assert_eq!(
            encode_frame(&frame),
            br#"{"jsonrpc":"2.0","id":1,"error":{"code":"not_found","message":"gone"}}"#
        );
        assert_eq!(
            decode_frame(&encode_frame(&frame)).expect("round trip"),
            frame
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
            json!({"jsonrpc": "1.0", "id": 1, "result": {}}),
        ] {
            let encoded = serde_json::to_vec(&value).expect("JSON fixture");
            assert!(decode_frame(&encoded).is_err(), "accepted {value}");
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
    fn method_inventory_is_sorted_and_unique() {
        assert!(
            SERVER_METHODS.is_sorted_by(|left, right| left < right),
            "SERVER_METHODS must stay sorted and duplicate-free for binary_search"
        );
        assert!(is_server_method("turn/start"));
        assert!(!is_server_method("turn/unknown"));
        for method in ["initialize", "initialized", "shutdown", "exit"] {
            assert!(
                !is_server_method(method),
                "{method} is a lifecycle method and must stay out of the inventory"
            );
        }
    }

    #[test]
    fn schema_reports_the_declared_version_and_methods() {
        let schema = protocol_schema();
        assert_eq!(schema["protocolVersion"], json!(PROTOCOL_VERSION));
        assert_eq!(
            schema["serverMethods"].as_array().map(Vec::len),
            Some(SERVER_METHODS.len())
        );
    }
}
