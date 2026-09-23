//! Client-side tools: capability scoping, registration, and input schemas.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use vibe_app_server::server::{ToolInvocation, ToolRegistry};

use super::RecordingClient;
use crate::client_tools::{AcpClientToolFactory, ClientTool};
use crate::protocol::{AcpClientCapabilities, AcpFilesystemCapabilities};

#[tokio::test]
async fn client_tool_factory_registers_capabilities_and_invokes_the_client_port() {
    let client = Arc::new(RecordingClient {
        calls: Mutex::new(Vec::new()),
    });
    let factory = AcpClientToolFactory {
        client: Some(client.clone()),
        capabilities: AcpClientCapabilities {
            fs: AcpFilesystemCapabilities {
                read_text_file: true,
                write_text_file: false,
            },
            terminal: true,
            session: Value::Null,
            meta: None,
        },
        timeout: Duration::from_secs(1),
    };
    let tools = ToolRegistry::default();

    vibe_app_server::server::SessionToolFactory::register(&factory, "session-1", &tools)
        .expect("client tools register");
    let names = tools
        .list()
        .expect("tool specs")
        .into_iter()
        .map(|spec| spec.name)
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        // The surface carries the registration order, not the name order.
        vec![
            "acp_read_text_file",
            "acp_terminal_create",
            "acp_terminal_output",
            "acp_terminal_wait_for_exit",
            "acp_terminal_kill",
            "acp_terminal_release",
        ]
    );

    let output = tools
        .invoke(
            "acp_read_text_file",
            ToolInvocation {
                call_id: "call-1".to_owned(),
                arguments: json!({"path": "src/lib.rs"}),
            },
        )
        .await
        .expect("client tool invocation");
    assert_eq!(output.typed_result, json!({"ok": true}));
    assert_eq!(
        client.calls.lock().expect("calls").as_slice(),
        ["fs/read_text_file"]
    );
}

/// Each tool declares the arguments its ACP method actually accepts. A schema
/// that flattened every method into "some required strings" would tell the
/// model that `terminal/create` takes neither arguments nor an environment.
#[test]
fn client_tool_schemas_describe_the_acp_methods_they_call() {
    let schema = |tool: ClientTool| {
        let factory = AcpClientToolFactory {
            client: None,
            capabilities: AcpClientCapabilities {
                fs: AcpFilesystemCapabilities {
                    read_text_file: true,
                    write_text_file: true,
                },
                terminal: true,
                session: Value::Null,
                meta: None,
            },
            timeout: Duration::from_secs(1),
        };
        let tools = ToolRegistry::default();
        vibe_app_server::server::SessionToolFactory::register(&factory, "session-1", &tools)
            .expect("client tools register");
        tools
            .list()
            .expect("tool specs")
            .into_iter()
            .find(|spec| spec.name == tool.tool())
            .expect("registered tool")
            .input_schema
    };

    let create = schema(ClientTool::TerminalCreate);
    assert_eq!(create["required"], json!(["command"]));
    assert_eq!(create["properties"]["args"]["type"], "array");
    assert_eq!(create["properties"]["args"]["items"]["type"], "string");
    assert_eq!(create["properties"]["env"]["items"]["type"], "object");
    assert_eq!(create["properties"]["cwd"]["type"], "string");
    assert_eq!(create["properties"]["outputByteLimit"]["type"], "integer");

    let read = schema(ClientTool::ReadTextFile);
    assert_eq!(read["required"], json!(["path"]));
    assert_eq!(read["properties"]["line"]["type"], "integer");
    assert_eq!(read["properties"]["limit"]["type"], "integer");

    let write = schema(ClientTool::WriteTextFile);
    assert_eq!(write["required"], json!(["path", "content"]));

    // `sessionId` is filled in by the handler, so no tool asks the model for it.
    assert!(ClientTool::ALL.into_iter().all(|tool| {
        schema(tool)["properties"].get("sessionId").is_none()
            && schema(tool)["additionalProperties"] == json!(false)
    }));
}
