//! Tools the agent executes through the connected ACP editor client.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use vibe_app_server::server::{
    OwnedToolHandlerFuture, SessionToolFactory, ToolAvailability, ToolError, ToolExecutionOutput,
    ToolInvocation, ToolOutputSink, ToolPresentationKind, ToolRegistry, ToolSource, ToolSpec,
};
use vibe_protocol::ClientToolCapability;

use crate::protocol::{AcpClientCapabilities, AcpError};

pub const DEFAULT_CLIENT_TOOL_TIMEOUT: Duration = Duration::from_secs(30);

pub type AcpClientFuture<'a> = Pin<Box<dyn Future<Output = Result<Value, String>> + Send + 'a>>;

pub trait AcpClientPort: Send + Sync {
    fn request<'a>(&'a self, method: &'a str, params: Value) -> AcpClientFuture<'a>;
}

/// Every capability the handshake can declare, in the order they are reported.
const CLIENT_TOOL_CAPABILITIES: [ClientToolCapability; 3] = [
    ClientToolCapability::FilesystemRead,
    ClientToolCapability::FilesystemWrite,
    ClientToolCapability::Terminal,
];

/// Whether the connected editor hosts `capability`.
///
/// This is the only place the ACP capability shape is read as a wire
/// capability, so the vocabulary the app-server gates on stays the vocabulary
/// the handshake published rather than a local retyping of it.
pub(crate) const fn capability_enabled(
    capability: ClientToolCapability,
    capabilities: &AcpClientCapabilities,
) -> bool {
    match capability {
        ClientToolCapability::FilesystemRead => capabilities.fs.read_text_file,
        ClientToolCapability::FilesystemWrite => capabilities.fs.write_text_file,
        ClientToolCapability::Terminal => capabilities.terminal,
    }
}

/// One ACP client method exposed as an agent tool. The enum is the single
/// source of truth for the tool name, the wire method, the capability that
/// gates it, and the arguments it accepts, so none of the four can be declared
/// without the other three.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClientTool {
    ReadTextFile,
    WriteTextFile,
    TerminalCreate,
    TerminalOutput,
    TerminalWaitForExit,
    TerminalKill,
    TerminalRelease,
}

impl ClientTool {
    pub(crate) const ALL: [Self; 7] = [
        Self::ReadTextFile,
        Self::WriteTextFile,
        Self::TerminalCreate,
        Self::TerminalOutput,
        Self::TerminalWaitForExit,
        Self::TerminalKill,
        Self::TerminalRelease,
    ];

    pub(crate) const fn tool(self) -> &'static str {
        match self {
            Self::ReadTextFile => "acp_read_text_file",
            Self::WriteTextFile => "acp_write_text_file",
            Self::TerminalCreate => "acp_terminal_create",
            Self::TerminalOutput => "acp_terminal_output",
            Self::TerminalWaitForExit => "acp_terminal_wait_for_exit",
            Self::TerminalKill => "acp_terminal_kill",
            Self::TerminalRelease => "acp_terminal_release",
        }
    }

    pub(crate) const fn method(self) -> &'static str {
        match self {
            Self::ReadTextFile => "fs/read_text_file",
            Self::WriteTextFile => "fs/write_text_file",
            Self::TerminalCreate => "terminal/create",
            Self::TerminalOutput => "terminal/output",
            Self::TerminalWaitForExit => "terminal/wait_for_exit",
            Self::TerminalKill => "terminal/kill",
            Self::TerminalRelease => "terminal/release",
        }
    }

    pub(crate) const fn capability(self) -> ClientToolCapability {
        match self {
            Self::ReadTextFile => ClientToolCapability::FilesystemRead,
            Self::WriteTextFile => ClientToolCapability::FilesystemWrite,
            Self::TerminalCreate
            | Self::TerminalOutput
            | Self::TerminalWaitForExit
            | Self::TerminalKill
            | Self::TerminalRelease => ClientToolCapability::Terminal,
        }
    }

    /// The arguments the ACP client method actually accepts. `sessionId` is
    /// not declared: the handler fills it in from the session the tool was
    /// registered for, so the model never supplies it.
    fn input_schema(self) -> Value {
        match self {
            Self::ReadTextFile => object_schema(
                json!({
                    "path": {"type": "string", "description": "Absolute path of the file to read"},
                    "line": {"type": "integer", "minimum": 0, "description": "First line to read, counting from 1"},
                    "limit": {"type": "integer", "minimum": 0, "description": "How many lines to read at most"},
                }),
                &["path"],
            ),
            Self::WriteTextFile => object_schema(
                json!({
                    "path": {"type": "string", "description": "Absolute path of the file to write"},
                    "content": {"type": "string", "description": "Full text to write to the file"},
                }),
                &["path", "content"],
            ),
            Self::TerminalCreate => object_schema(
                json!({
                    "command": {"type": "string", "description": "Program to run"},
                    "args": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Arguments passed to the program",
                    },
                    "env": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "name": {"type": "string"},
                                "value": {"type": "string"},
                            },
                            "required": ["name", "value"],
                        },
                        "description": "Environment variables set for the program",
                    },
                    "cwd": {"type": "string", "description": "Absolute directory to run the program in"},
                    "outputByteLimit": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "How many output bytes the client retains before truncating from the start",
                    },
                }),
                &["command"],
            ),
            Self::TerminalOutput
            | Self::TerminalWaitForExit
            | Self::TerminalKill
            | Self::TerminalRelease => object_schema(
                json!({
                    "terminalId": {
                        "type": "string",
                        "description": "Identifier a previous terminal creation returned",
                    },
                }),
                &["terminalId"],
            ),
        }
    }
}

fn object_schema(properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}

pub(crate) fn client_tool_for_method(method: &str) -> Option<ClientTool> {
    ClientTool::ALL
        .into_iter()
        .find(|tool| tool.method() == method)
}

pub(crate) fn declared_client_tools(
    capabilities: &AcpClientCapabilities,
) -> Vec<ClientToolCapability> {
    CLIENT_TOOL_CAPABILITIES
        .into_iter()
        .filter(|capability| capability_enabled(*capability, capabilities))
        .collect()
}

pub(crate) struct AcpClientToolFactory {
    pub(crate) client: Option<Arc<dyn AcpClientPort>>,
    pub(crate) capabilities: AcpClientCapabilities,
    pub(crate) timeout: Duration,
}

impl SessionToolFactory for AcpClientToolFactory {
    fn register(&self, session_id: &str, tools: &ToolRegistry) -> Result<(), String> {
        for tool in ClientTool::ALL
            .into_iter()
            .filter(|tool| capability_enabled(tool.capability(), &self.capabilities))
        {
            tools
                .register(
                    ToolSpec {
                        name: tool.tool().to_owned(),
                        description: format!(
                            "Execute `{}` through the connected ACP editor client",
                            tool.method()
                        ),
                        input_schema: tool.input_schema(),
                        output_schema: None,
                        config: Value::Null,
                        state: Value::Null,
                        availability: ToolAvailability::Available,
                        presentation: ToolPresentationKind::Generic,
                        source: ToolSource::BuiltIn,
                        selection_priority: 30,
                    },
                    client_tool_handler(
                        self.client.clone(),
                        tool.method(),
                        session_id.to_owned(),
                        self.timeout,
                    ),
                )
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }
}

fn client_tool_handler(
    client: Option<Arc<dyn AcpClientPort>>,
    method: &'static str,
    session_id: String,
    timeout: Duration,
) -> Arc<impl Fn(&ToolInvocation, ToolOutputSink) -> OwnedToolHandlerFuture + Send + Sync> {
    Arc::new(
        move |invocation: &ToolInvocation, _output: ToolOutputSink| -> OwnedToolHandlerFuture {
            let client = client.clone();
            let session_id = session_id.clone();
            let mut params = invocation.arguments.clone();
            Box::pin(async move {
                let Some(client) = client else {
                    return Err(ToolError::Unavailable(
                        "ACP client transport is unavailable".to_owned(),
                    ));
                };
                let object = params.as_object_mut().ok_or_else(|| {
                    ToolError::Execution("ACP client tool input must be an object".to_owned())
                })?;
                object.insert("sessionId".to_owned(), json!(session_id));
                let result = tokio::time::timeout(timeout, client.request(method, params))
                    .await
                    .map_err(|_| {
                        ToolError::Execution(format!("ACP client tool `{method}` timed out"))
                    })?
                    .map_err(ToolError::Execution)?;
                let model_text = serde_json::to_string(&result)
                    .map_err(|error| ToolError::InvalidResult(error.to_string()))?;
                Ok(ToolExecutionOutput {
                    typed_result: result,
                    model_text,
                    display: json!({"kind": "client_tool", "method": method}),
                    chunks: Vec::new(),
                })
            })
        },
    )
}

/// Client methods the agent may call directly, gated by the same table the
/// tool registry uses.
pub(crate) fn require_client_method(
    method: &str,
    capabilities: &AcpClientCapabilities,
) -> Result<(), AcpError> {
    let tool = client_tool_for_method(method)
        .ok_or_else(|| AcpError::UnsupportedClientFlow(method.to_owned()))?;
    if capability_enabled(tool.capability(), capabilities) {
        Ok(())
    } else {
        Err(AcpError::UnsupportedClientFlow(format!(
            "client did not advertise `{method}`"
        )))
    }
}
