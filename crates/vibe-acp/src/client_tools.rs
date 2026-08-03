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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClientCapability {
    FilesystemRead,
    FilesystemWrite,
    Terminal,
}

impl ClientCapability {
    const ALL: [Self; 3] = [Self::FilesystemRead, Self::FilesystemWrite, Self::Terminal];

    pub(crate) const fn enabled(self, capabilities: &AcpClientCapabilities) -> bool {
        match self {
            Self::FilesystemRead => capabilities.fs.read_text_file,
            Self::FilesystemWrite => capabilities.fs.write_text_file,
            Self::Terminal => capabilities.terminal,
        }
    }

    const fn declaration(self) -> ClientToolCapability {
        match self {
            Self::FilesystemRead => ClientToolCapability::FilesystemRead,
            Self::FilesystemWrite => ClientToolCapability::FilesystemWrite,
            Self::Terminal => ClientToolCapability::Terminal,
        }
    }
}

/// One ACP client method exposed as an agent tool. This table is the single
/// source of truth for the method name, the capability that gates it, and the
/// arguments it accepts.
pub(crate) struct ClientToolSpec {
    pub(crate) tool: &'static str,
    pub(crate) method: &'static str,
    pub(crate) capability: ClientCapability,
    fields: &'static [&'static str],
}

impl ClientToolSpec {
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": self
                .fields
                .iter()
                .map(|field| ((*field).to_owned(), json!({"type": "string"})))
                .collect::<serde_json::Map<_, _>>(),
            "required": self.fields,
            "additionalProperties": true,
        })
    }
}

pub(crate) const CLIENT_TOOLS: &[ClientToolSpec] = &[
    ClientToolSpec {
        tool: "acp_read_text_file",
        method: "fs/read_text_file",
        capability: ClientCapability::FilesystemRead,
        fields: &["path"],
    },
    ClientToolSpec {
        tool: "acp_write_text_file",
        method: "fs/write_text_file",
        capability: ClientCapability::FilesystemWrite,
        fields: &["path", "content"],
    },
    ClientToolSpec {
        tool: "acp_terminal_create",
        method: "terminal/create",
        capability: ClientCapability::Terminal,
        fields: &["command"],
    },
    ClientToolSpec {
        tool: "acp_terminal_output",
        method: "terminal/output",
        capability: ClientCapability::Terminal,
        fields: &["terminalId"],
    },
    ClientToolSpec {
        tool: "acp_terminal_wait_for_exit",
        method: "terminal/wait_for_exit",
        capability: ClientCapability::Terminal,
        fields: &["terminalId"],
    },
    ClientToolSpec {
        tool: "acp_terminal_kill",
        method: "terminal/kill",
        capability: ClientCapability::Terminal,
        fields: &["terminalId"],
    },
    ClientToolSpec {
        tool: "acp_terminal_release",
        method: "terminal/release",
        capability: ClientCapability::Terminal,
        fields: &["terminalId"],
    },
];

pub(crate) fn client_tool_spec(method: &str) -> Option<&'static ClientToolSpec> {
    CLIENT_TOOLS.iter().find(|spec| spec.method == method)
}

pub(crate) fn declared_client_tools(
    capabilities: &AcpClientCapabilities,
) -> Vec<ClientToolCapability> {
    ClientCapability::ALL
        .into_iter()
        .filter(|capability| capability.enabled(capabilities))
        .map(ClientCapability::declaration)
        .collect()
}

pub(crate) struct AcpClientToolFactory {
    pub(crate) client: Option<Arc<dyn AcpClientPort>>,
    pub(crate) capabilities: AcpClientCapabilities,
    pub(crate) timeout: Duration,
}

impl SessionToolFactory for AcpClientToolFactory {
    fn register(&self, session_id: &str, tools: &ToolRegistry) -> Result<(), String> {
        for spec in CLIENT_TOOLS
            .iter()
            .filter(|spec| spec.capability.enabled(&self.capabilities))
        {
            tools
                .register(
                    ToolSpec {
                        name: spec.tool.to_owned(),
                        description: format!(
                            "Execute `{}` through the connected ACP editor client",
                            spec.method
                        ),
                        input_schema: spec.input_schema(),
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
                        spec.method,
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
    let spec = client_tool_spec(method)
        .ok_or_else(|| AcpError::UnsupportedClientFlow(method.to_owned()))?;
    if spec.capability.enabled(capabilities) {
        Ok(())
    } else {
        Err(AcpError::UnsupportedClientFlow(format!(
            "client did not advertise `{method}`"
        )))
    }
}
