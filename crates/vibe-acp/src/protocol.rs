//! Wire types and errors for the ACP surface.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use vibe_app_server::client::ClientError;

pub const ACP_PROTOCOL_VERSION: u16 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AcpInitializeRequest {
    #[serde(default = "default_protocol_version")]
    pub protocol_version: u16,
    #[serde(default)]
    pub client_capabilities: AcpClientCapabilities,
    #[serde(default)]
    pub client_info: Option<AcpClientInfo>,
    #[serde(default, rename = "_meta")]
    pub meta: Option<Value>,
}

impl Default for AcpInitializeRequest {
    fn default() -> Self {
        Self {
            protocol_version: ACP_PROTOCOL_VERSION,
            client_capabilities: AcpClientCapabilities::default(),
            client_info: None,
            meta: None,
        }
    }
}

const fn default_protocol_version() -> u16 {
    ACP_PROTOCOL_VERSION
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct AcpClientCapabilities {
    pub fs: AcpFilesystemCapabilities,
    pub terminal: bool,
    pub session: Value,
    #[serde(rename = "_meta")]
    pub meta: Option<Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct AcpFilesystemCapabilities {
    pub read_text_file: bool,
    pub write_text_file: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcpClientInfo {
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub title: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AcpInitializeResponse {
    pub protocol_version: u16,
    pub agent_capabilities: AcpAgentCapabilities,
    pub auth_methods: Vec<Value>,
    pub agent_info: AcpImplementation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AcpAgentCapabilities {
    pub load_session: bool,
    pub prompt_capabilities: AcpPromptCapabilities,
    pub session_capabilities: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AcpPromptCapabilities {
    pub audio: bool,
    pub embedded_context: bool,
    pub image: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcpImplementation {
    pub name: String,
    pub title: String,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AcpNewSession {
    pub cwd: String,
    #[serde(default)]
    pub additional_directories: Option<Vec<String>>,
    #[serde(default)]
    pub mcp_servers: Vec<Value>,
    #[serde(default, rename = "_meta")]
    pub meta: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AcpLoadSession {
    pub session_id: String,
    pub cwd: String,
    #[serde(default)]
    pub additional_directories: Vec<String>,
    #[serde(default)]
    pub mcp_servers: Vec<Value>,
    #[serde(default, rename = "_meta")]
    pub meta: Option<Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct AcpListSessions {
    pub cwd: Option<String>,
    pub cursor: Option<String>,
    #[serde(rename = "_meta")]
    pub meta: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AcpForkSession {
    pub session_id: String,
    pub cwd: String,
    #[serde(default)]
    pub new_session_id: Option<String>,
    #[serde(default)]
    pub message_id: Option<String>,
    #[serde(default)]
    pub additional_directories: Vec<String>,
    #[serde(default)]
    pub mcp_servers: Vec<Value>,
    #[serde(default, rename = "_meta")]
    pub meta: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AcpSession {
    pub session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modes: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_options: Option<Vec<Value>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AcpSessionInfo {
    pub session_id: String,
    pub cwd: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub additional_directories: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    #[serde(rename = "_meta", skip_serializing_if = "BTreeMap::is_empty")]
    pub meta: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AcpSessionList {
    pub sessions: Vec<AcpSessionInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AcpHistoryPage {
    pub entries: Vec<Value>,
    pub next_offset: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AcpSessionUpdate {
    pub session_id: String,
    pub update: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AcpUsage {
    pub total_tokens: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AcpPromptResponse {
    pub stop_reason: String,
    pub usage: AcpUsage,
}

#[derive(Debug, Error)]
pub enum AcpError {
    #[error("ACP agent is not initialized")]
    NotInitialized,
    #[error("ACP agent is already initialized")]
    AlreadyInitialized,
    #[error("ACP protocol version {0} is not supported")]
    UnsupportedProtocol(u16),
    #[error("ACP authentication method `{0}` is not supported")]
    UnsupportedAuthentication(String),
    #[error("ACP session `{0}` was not found")]
    SessionNotFound(String),
    #[error("ACP session `{0}` already exists")]
    SessionConflict(String),
    #[error("invalid ACP parameters: {0}")]
    InvalidParams(String),
    #[error("invalid ACP response: {0}")]
    InvalidResponse(String),
    #[error("ACP client flow is unsupported: {0}")]
    UnsupportedClientFlow(String),
    #[error("ACP client tool `{0}` timed out")]
    ClientToolTimeout(String),
    #[error("ACP client tool failed: {0}")]
    ClientTool(String),
    #[error("ACP driver failed: {0}")]
    Driver(String),
    #[error("ACP configuration failed: {0}")]
    Configuration(String),
    #[error("ACP state lock is poisoned")]
    StatePoisoned,
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Client(#[from] ClientError),
    #[error("ACP client disconnected")]
    Disconnected,
    #[error("ACP update queue is saturated")]
    Backpressure,
}

impl AcpError {
    /// JSON-RPC error code carried on the wire. The match stays exhaustive so a
    /// new variant cannot silently inherit the internal-error code.
    #[must_use]
    pub fn json_rpc_code(&self) -> i64 {
        match self {
            Self::UnsupportedClientFlow(_) => -32601,
            Self::InvalidParams(_)
            | Self::Json(_)
            | Self::UnsupportedProtocol(_)
            | Self::UnsupportedAuthentication(_) => -32602,
            Self::SessionNotFound(_) => -32001,
            Self::SessionConflict(_) | Self::AlreadyInitialized => -32002,
            Self::NotInitialized => -32003,
            Self::Disconnected => -32004,
            Self::Backpressure => -32005,
            Self::InvalidResponse(_)
            | Self::ClientToolTimeout(_)
            | Self::ClientTool(_)
            | Self::Driver(_)
            | Self::Configuration(_)
            | Self::StatePoisoned
            | Self::Client(_) => -32603,
        }
    }
}
