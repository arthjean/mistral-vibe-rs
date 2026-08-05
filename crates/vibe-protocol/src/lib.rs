//! Wire contract for the Mistral Vibe app-server.
//!
//! The crate owns three things and nothing else: the JSON-RPC envelopes that
//! cross the transport, the inventory of methods the server answers, and the
//! handshake payloads exchanged during `initialize`.
//!
//! Envelopes are deliberately stricter than JSON-RPC 2.0: every struct denies
//! unknown fields, which is what lets [`Envelope`] discriminate its variants
//! while staying untagged. Relaxing `deny_unknown_fields` on any of them makes
//! the declaration order of [`Envelope`] silently load-bearing.

#![warn(missing_docs)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use std::collections::BTreeMap;

use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

/// Every method the app-server routes, sorted and unique.
///
/// Lifecycle methods (`initialize`, `initialized`, `shutdown`, `exit`) are
/// deliberately absent: they are handled before method dispatch and are not
/// part of the negotiated surface.
pub const SERVER_METHODS: [&str; 82] = [
    "account/read",
    "agents/install",
    "agents/list",
    "agents/uninstall",
    "callback/respond",
    "config/batchWrite",
    "config/fields/read",
    "config/patch",
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

/// Correlates a request with its response.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum RequestId {
    /// Numeric identifier.
    Integer(i64),
    /// Opaque string identifier.
    String(String),
}

/// Closed set of failure codes carried by [`ProtocolError`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProtocolErrorCode {
    /// The frame is well-formed JSON but violates the lifecycle contract.
    InvalidRequest,
    /// The method exists but its parameters do not deserialize.
    InvalidParams,
    /// A method was called before the handshake completed.
    NotInitialized,
    /// The addressed resource does not exist.
    NotFound,
    /// The request conflicts with the current server state.
    Conflict,
    /// The addressed turn is no longer the active one.
    StaleTurn,
    /// The active turn cannot accept steering input.
    NotSteerable,
    /// Compaction did not produce a usable session.
    CompactionFailed,
    /// Credentials are missing or rejected.
    Unauthorized,
    /// Credentials are valid but the operation is not permitted.
    Forbidden,
    /// The method is not part of [`SERVER_METHODS`].
    MethodNotFound,
    /// The server failed for a reason the client cannot act on.
    InternalError,
}

/// Error payload of an [`ErrorResponse`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProtocolError {
    /// Machine-readable failure class.
    pub code: ProtocolErrorCode,
    /// Human-readable explanation.
    pub message: String,
    /// Optional structured detail; omitted from the wire when null.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub data: Value,
}

/// A message that expects no response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Notification {
    /// Always `2.0`.
    pub jsonrpc: JsonRpcVersion,
    /// Notification name.
    pub method: String,
    /// Payload; absent on the wire is equivalent to empty, per JSON-RPC 2.0.
    #[serde(default)]
    pub params: BTreeMap<String, Value>,
}

/// A message that expects a [`SuccessResponse`] or an [`ErrorResponse`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ServerRequest {
    /// Always `2.0`.
    pub jsonrpc: JsonRpcVersion,
    /// Identifier echoed back in the response.
    pub id: RequestId,
    /// One of [`SERVER_METHODS`], or a lifecycle method.
    pub method: String,
    /// Payload; absent on the wire is equivalent to empty, per JSON-RPC 2.0.
    #[serde(default)]
    pub params: BTreeMap<String, Value>,
}

/// Successful answer to a [`ServerRequest`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SuccessResponse {
    /// Always `2.0`.
    pub jsonrpc: JsonRpcVersion,
    /// Identifier of the answered request.
    pub id: RequestId,
    /// Method-specific payload. Required: a response carries `result` or
    /// `error`, never neither.
    pub result: BTreeMap<String, Value>,
}

/// Failed answer to a [`ServerRequest`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ErrorResponse {
    /// Always `2.0`.
    pub jsonrpc: JsonRpcVersion,
    /// Identifier of the answered request.
    pub id: RequestId,
    /// Failure detail.
    pub error: ProtocolError,
}

/// The only JSON-RPC version this protocol speaks.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum JsonRpcVersion {
    /// Serializes as `"2.0"`.
    #[default]
    #[serde(rename = "2.0")]
    V2,
}

/// Any frame that can cross the transport, in either direction.
///
/// Variants are discriminated by the fields each one denies, not by ordering.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum Envelope {
    /// See [`Notification`].
    Notification(Notification),
    /// See [`ServerRequest`].
    Request(ServerRequest),
    /// See [`SuccessResponse`].
    Success(SuccessResponse),
    /// See [`ErrorResponse`].
    Error(ErrorResponse),
}

/// Why an inbound frame could not be turned into an [`Envelope`].
#[derive(Debug, Error)]
pub enum ProtocolValidationError {
    /// The bytes are not a valid frame. Untagged decoding cannot report which
    /// variant was intended, so the message names the whole envelope.
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
        "protocolVersion": ProtocolVersion::V1,
        "serverMethods": &SERVER_METHODS[..],
        "envelope": schema_for!(Envelope),
        "initializeParams": schema_for!(InitializeParams),
        "initializeResponse": schema_for!(InitializeResponse),
        "sessionMcpServer": schema_for!(SessionMcpServer),
        "protocolError": schema_for!(ProtocolError),
    })
}

/// How the client process was launched.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ClientEntrypoint {
    /// Not reported by the client.
    #[default]
    Unknown,
    /// Interactive terminal client.
    Cli,
    /// Agent Client Protocol bridge.
    Acp,
    /// Embedded or scripted client.
    Programmatic,
}

/// Terminal the client is attached to, when it can be identified.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TerminalEmulator {
    /// Not reported, or not recognized.
    #[default]
    Unknown,
    /// Visual Studio Code integrated terminal.
    Vscode,
    /// Visual Studio Code Insiders integrated terminal.
    VscodeInsiders,
    /// Cursor integrated terminal.
    Cursor,
    /// JetBrains IDE integrated terminal.
    Jetbrains,
    /// macOS Terminal.app.
    AppleTerminal,
    /// iTerm2.
    Iterm2,
    /// WezTerm.
    Wezterm,
    /// Ghostty.
    Ghostty,
    /// Alacritty.
    Alacritty,
    /// Kitty.
    Kitty,
    /// Hyper.
    Hyper,
    /// Windows Terminal.
    WindowsTerminal,
}

/// Identity the client declares during `initialize`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ClientInfo {
    /// Program name.
    pub name: String,
    /// Program version.
    pub version: String,
    /// Display name, when it differs from `name`.
    #[serde(default)]
    pub title: Option<String>,
    /// How the client was launched.
    #[serde(default)]
    pub entrypoint: ClientEntrypoint,
    /// Terminal the client runs in.
    #[serde(default)]
    pub terminal_emulator: TerminalEmulator,
}

/// Kinds of server-initiated callback a client can answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CallbackKind {
    /// Approve or deny a tool effect.
    Approval,
    /// Supply free-form input to the running turn.
    UserInput,
    /// Complete a connector authorization flow.
    ConnectorAuth,
}

/// Tools the server may delegate to the client process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum ClientToolCapability {
    /// Read files through the client.
    #[serde(rename = "filesystem/read")]
    FilesystemRead,
    /// Write files through the client.
    #[serde(rename = "filesystem/write")]
    FilesystemWrite,
    /// Run terminal commands through the client.
    #[serde(rename = "terminal")]
    Terminal,
}

/// What the client can handle, declared during `initialize`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ClientCapabilities {
    /// Callback kinds the client answers. The server refuses to raise a
    /// callback the client did not declare.
    #[serde(default)]
    pub callback_kinds: Vec<CallbackKind>,
    /// Tools the client exposes back to the server.
    #[serde(default)]
    pub client_tools: Vec<ClientToolCapability>,
}

/// Parameters of the `initialize` request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct InitializeParams {
    /// Who is connecting.
    pub client_info: ClientInfo,
    /// What the client can handle.
    #[serde(default)]
    pub capabilities: ClientCapabilities,
}

/// Identity the server reports during `initialize`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ServerInfo {
    /// Program name.
    pub name: String,
    /// Program version.
    pub version: String,
}

/// How a connection is carried.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TransportKind {
    /// Client and server share a process.
    InProcess,
    /// Newline-delimited frames over stdin and stdout.
    Stdio,
}

/// What the server offers, reported during `initialize`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ServerCapabilities {
    /// Methods this build actually routes, a subset of [`SERVER_METHODS`].
    #[serde(default)]
    pub methods: Vec<String>,
    /// Callback kinds this build can raise.
    #[serde(default)]
    pub callback_kinds: Vec<CallbackKind>,
    /// Transport carrying this connection.
    #[serde(default)]
    pub transports: Vec<TransportKind>,
}

/// Result of the `initialize` request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct InitializeResponse {
    /// Who answered.
    pub server_info: ServerInfo,
    /// Contract version spoken by the server.
    pub protocol_version: ProtocolVersion,
    /// What the server offers.
    pub capabilities: ServerCapabilities,
}

/// The only contract version this crate describes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum ProtocolVersion {
    /// Serializes as `"1"`.
    #[default]
    #[serde(rename = "1")]
    V1,
}

/// MCP server a session can attach, keyed by its transport.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "transport", rename_all = "kebab-case")]
pub enum SessionMcpServer {
    /// Remote server reached over streamable HTTP.
    StreamableHttp {
        /// Display name.
        name: String,
        /// Endpoint URL.
        url: String,
        /// Extra headers sent with every call.
        #[serde(default)]
        headers: BTreeMap<String, String>,
    },
    /// Local server spawned as a child process.
    Stdio {
        /// Display name.
        name: String,
        /// Executable to spawn.
        command: String,
        /// Arguments passed to `command`.
        #[serde(default)]
        args: Vec<String>,
        /// Environment overrides for the child.
        #[serde(default)]
        env: BTreeMap<String, String>,
        /// Working directory for the child.
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
    fn camel_case_is_strict_and_variants_are_closed() {
        let camel = json!({
            "clientInfo": {
                "name": "test",
                "version": "1",
                "title": null,
                "entrypoint": "unknown",
                "terminalEmulator": "vscode"
            }
        });
        let parsed = serde_json::from_value::<InitializeParams>(camel).expect("camelCase params");
        assert_eq!(
            parsed.client_info.terminal_emulator,
            TerminalEmulator::Vscode
        );
        assert_eq!(parsed.capabilities, ClientCapabilities::default());

        let snake = json!({
            "client_info": {"name": "test", "version": "1"}
        });
        assert!(serde_json::from_value::<InitializeParams>(snake).is_err());
        assert!(serde_json::from_value::<CallbackKind>(json!("unknown")).is_err());
    }

    #[test]
    fn session_mcp_servers_round_trip_by_transport() {
        let stdio = json!({
            "transport": "stdio",
            "name": "local",
            "command": "server",
            "args": ["--flag"],
            "env": {"KEY": "value"},
            "cwd": null
        });
        let parsed = serde_json::from_value::<SessionMcpServer>(stdio.clone()).expect("stdio");
        assert!(matches!(parsed, SessionMcpServer::Stdio { .. }));
        assert_eq!(serde_json::to_value(&parsed).expect("re-encode"), stdio);

        let http = json!({
            "transport": "streamable-http",
            "name": "remote",
            "url": "https://example.test/mcp",
            "headers": {}
        });
        assert!(matches!(
            serde_json::from_value::<SessionMcpServer>(http).expect("http"),
            SessionMcpServer::StreamableHttp { .. }
        ));
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
        assert_eq!(schema["protocolVersion"], json!("1"));
        assert_eq!(
            schema["serverMethods"].as_array().map(Vec::len),
            Some(SERVER_METHODS.len())
        );
    }
}
