//! Wire types and errors for the ACP surface.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use thiserror::Error;
use vibe_app_server::client::ClientError;

pub const ACP_PROTOCOL_VERSION: u16 = 1;

/// What `initialize` carried, read the way the validator already accepted it:
/// the version is not checked and the booleans take the lax spellings.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AcpInitializeRequest {
    pub client_capabilities: Option<AcpClientCapabilities>,
    pub client_info: Option<AcpClientInfo>,
}

impl AcpInitializeRequest {
    #[must_use]
    pub fn from_params(params: &Value) -> Self {
        let capabilities = params
            .get("clientCapabilities")
            .filter(|value| value.is_object())
            .map(AcpClientCapabilities::from_value);
        let client_info = params
            .get("clientInfo")
            .filter(|value| value.is_object())
            .map(|info| AcpClientInfo {
                name: info
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                version: info
                    .get("version")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                title: info
                    .get("title")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned),
            });
        Self {
            client_capabilities: capabilities,
            client_info,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AcpClientCapabilities {
    pub fs: AcpFilesystemCapabilities,
    pub terminal: bool,
    /// Whether the client renders a form elicitation, which is what lets a
    /// session ask the user a question.
    pub elicitation_form: bool,
    pub meta: Option<Value>,
}

impl AcpClientCapabilities {
    fn from_value(value: &Value) -> Self {
        let flag =
            |value: Option<&Value>| value.and_then(crate::validation::lax_bool).unwrap_or(false);
        let fs = value.get("fs").filter(|fs| fs.is_object());
        Self {
            fs: AcpFilesystemCapabilities {
                read_text_file: flag(fs.and_then(|fs| fs.get("readTextFile"))),
                write_text_file: flag(fs.and_then(|fs| fs.get("writeTextFile"))),
            },
            terminal: flag(value.get("terminal")),
            elicitation_form: value
                .get("elicitation")
                .and_then(|elicitation| elicitation.get("form"))
                .is_some_and(Value::is_object),
            meta: value.get("_meta").filter(|meta| meta.is_object()).cloned(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
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
#[serde(rename_all = "camelCase")]
pub struct AcpNewSession {
    pub cwd: String,
    #[serde(default)]
    pub additional_directories: Option<Vec<String>>,
    #[serde(default)]
    pub mcp_servers: Option<Vec<Value>>,
    #[serde(default, rename = "_meta")]
    pub meta: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AcpLoadSession {
    pub session_id: String,
    pub cwd: String,
    #[serde(default)]
    pub additional_directories: Option<Vec<String>>,
    #[serde(default)]
    pub mcp_servers: Option<Vec<Value>>,
    #[serde(default, rename = "_meta")]
    pub meta: Option<Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AcpListSessions {
    pub cwd: Option<String>,
    pub cursor: Option<String>,
    #[serde(rename = "_meta")]
    pub meta: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AcpForkSession {
    pub session_id: String,
    pub cwd: String,
    #[serde(default)]
    pub new_session_id: Option<String>,
    #[serde(default)]
    pub message_id: Option<String>,
    #[serde(default)]
    pub additional_directories: Option<Vec<String>>,
    #[serde(default)]
    pub mcp_servers: Option<Vec<Value>>,
    #[serde(default, rename = "_meta")]
    pub meta: Option<Value>,
}

/// The payload the `telemetry/send` extension notification carries.
///
/// Reference `TelemetryNotification` (`vibe/acp/agent.py:215-220`): the editor
/// names the event, carries its own properties and the session they belong to.
/// A field the model does not declare is ignored rather than refused, which is
/// what its `extra="ignore"` config does.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AcpTelemetryNotification {
    pub event: String,
    #[serde(default)]
    pub properties: Map<String, Value>,
    pub session_id: String,
}

/// What a session's negotiated settings look like on the wire. Both the
/// `session/new` and `session/load` responses carry exactly this, which is why
/// neither one owns the shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AcpSessionSettings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modes: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_options: Option<Vec<Value>>,
}

/// The `session/load` response, which ACP declares without an identity: the
/// client already named the session it asked to load.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AcpLoadedSession {
    #[serde(flatten)]
    pub settings: AcpSessionSettings,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AcpSessionUpdate {
    pub session_id: String,
    pub update: Value,
}

/// Every failure an ACP request can answer with.
///
/// The codes and the `data` payloads are the reference's contract
/// (`vibe/acp/exceptions.py`, plus the JSON-RPC errors its router raises in
/// the `acp` package); the messages are this port's own.
#[derive(Debug, Error)]
pub enum AcpError {
    /// The router knows no such standard method.
    #[error("Method not found")]
    MethodNotFound(String),
    /// An extension method family has no member by this name.
    #[error("the `{0}` extension is not served by this agent")]
    NotImplemented(String),
    /// The parameters did not validate against the request schema; each item
    /// is one error in the validator's own shape.
    #[error("Invalid params")]
    Validation(Vec<Value>),
    #[error("{0}")]
    Unauthenticated(String),
    /// A request the agent understood but refuses, the reference's
    /// `InvalidRequestError`.
    #[error("{0}")]
    InvalidParams(String),
    #[error("no live session is named `{0}`")]
    SessionNotFound(String),
    #[error(
        "the {provider} provider is rate limiting requests to {model}; wait a moment and retry"
    )]
    RateLimited { provider: String, model: String },
    #[error("{0}")]
    Configuration(String),
    #[error("{0}")]
    ConversationLimit(String),
    #[error(
        "the conversation no longer fits the context window of {model} on {provider}; rewind recent steps with /rewind, then summarize them with /compact"
    )]
    ContextTooLong { provider: String, model: String },
    #[error("{model} on {provider} refused to answer{}: {}", category.as_deref().map(|category| format!(" ({category})")).unwrap_or_default(), explanation.as_deref().unwrap_or("rephrase the request or open a new conversation"))]
    Refusal {
        provider: String,
        model: String,
        category: Option<String>,
        explanation: Option<String>,
    },
    #[error("{detail}")]
    CompactionFailed { reason: String, detail: String },
    #[error("{detail}")]
    InvalidImage { detail: String, reason: String },
    #[error(
        "the model `{0}` cannot read images; pick another model or enable image input for this one"
    )]
    ImagesNotSupported(String),
    /// The reference's `InternalError`: a failure it reports with a message
    /// and no data.
    #[error("{0}")]
    Internal(String),
    /// An error the connected client answered one of our requests with, which
    /// the reference lets propagate unchanged.
    #[error("{message}")]
    Client {
        code: i64,
        message: String,
        data: Value,
    },
    #[error("ACP authentication method `{0}` is not supported")]
    UnsupportedAuthentication(String),
    #[error("ACP authentication failed: {0}")]
    AuthFailure(String),
    #[error("invalid ACP response: {0}")]
    InvalidResponse(String),
    #[error("ACP client tool `{0}` timed out")]
    ClientToolTimeout(String),
    #[error("ACP client tool failed: {0}")]
    ClientTool(String),
    #[error("{0}")]
    Driver(String),
    /// A failure the reference lets escape its handler, which its connection
    /// answers as `Internal error` with the text under `details`.
    #[error("{0}")]
    Unexpected(String),
    #[error("ACP state lock is poisoned")]
    StatePoisoned,
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    AppServer(#[from] ClientError),
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
            Self::Unauthenticated(_) => -32_000,
            Self::MethodNotFound(_) | Self::NotImplemented(_) => -32_601,
            Self::Validation(_)
            | Self::InvalidParams(_)
            | Self::SessionNotFound(_)
            | Self::UnsupportedAuthentication(_) => -32_602,
            Self::RateLimited { .. } => -31_001,
            Self::Configuration(_) => -31_002,
            Self::ConversationLimit(_) => -31_003,
            Self::ContextTooLong { .. } => -31_004,
            Self::Refusal { .. } => -31_005,
            Self::CompactionFailed { .. } => -31_006,
            Self::InvalidImage { .. } => -31_007,
            Self::ImagesNotSupported(_) => -31_008,
            Self::Client { code, .. } => *code,
            Self::Internal(_)
            | Self::AuthFailure(_)
            | Self::Driver(_)
            | Self::Unexpected(_)
            | Self::InvalidResponse(_)
            | Self::ClientToolTimeout(_)
            | Self::ClientTool(_)
            | Self::StatePoisoned
            | Self::Json(_)
            | Self::AppServer(_)
            | Self::Disconnected
            | Self::Backpressure => -32_603,
        }
    }

    /// The `data` member of the error object, which the reference always
    /// publishes, as `null` when the failure carries none.
    #[must_use]
    pub fn json_rpc_data(&self) -> Value {
        match self {
            Self::MethodNotFound(method) | Self::NotImplemented(method) => {
                serde_json::json!({"method": method})
            }
            Self::Validation(errors) => serde_json::json!({"errors": errors}),
            Self::SessionNotFound(session_id) => serde_json::json!({"session_id": session_id}),
            Self::RateLimited { provider, model } | Self::ContextTooLong { provider, model } => {
                serde_json::json!({"provider": provider, "model": model})
            }
            Self::Refusal {
                provider,
                model,
                category,
                explanation,
            } => serde_json::json!({
                "provider": provider,
                "model": model,
                "category": category,
                "explanation": explanation,
            }),
            Self::CompactionFailed { reason, .. } | Self::InvalidImage { reason, .. } => {
                serde_json::json!({"reason": reason})
            }
            Self::Client { data, .. } => data.clone(),
            // A failure the reference never anticipated reaches its connection
            // as a bare exception, which answers with the text under `details`.
            Self::Unexpected(_)
            | Self::InvalidResponse(_)
            | Self::ClientToolTimeout(_)
            | Self::ClientTool(_)
            | Self::StatePoisoned
            | Self::Json(_)
            | Self::AppServer(_)
            | Self::Disconnected
            | Self::Backpressure => serde_json::json!({"details": self.to_string()}),
            Self::Unauthenticated(_)
            | Self::InvalidParams(_)
            | Self::UnsupportedAuthentication(_)
            | Self::Configuration(_)
            | Self::ConversationLimit(_)
            | Self::ImagesNotSupported(_)
            | Self::Internal(_)
            | Self::AuthFailure(_)
            | Self::Driver(_) => Value::Null,
        }
    }

    /// The message of the error object. An unanticipated failure is reported
    /// the way the reference's connection reports a bare exception.
    #[must_use]
    pub fn json_rpc_message(&self) -> String {
        match self.json_rpc_data() {
            Value::Object(fields) if fields.contains_key("details") => "Internal error".to_owned(),
            _ => self.to_string(),
        }
    }

    /// The complete JSON-RPC error object.
    #[must_use]
    pub fn json_rpc_error(&self) -> Value {
        serde_json::json!({
            "code": self.json_rpc_code(),
            "message": self.json_rpc_message(),
            "data": self.json_rpc_data(),
        })
    }
}
