//! The server's tests, and the harness they share.
//!
//! Every group below drives a real `ServerConnection` through the same
//! `initialize`, `start_session` and `call` helpers, so a test reads as the
//! sequence of requests a client would send.

pub use super::*;
use std::fs;
use vibe_core::events::ApprovalDecisionType;

mod callback_tests;
mod profile_tests;
mod resource_tests;
mod session_tests;
mod startup_tests;
mod turn_tests;
/// `SERVER_METHODS` is the reference contract, not this build's routing
/// table: a name belongs there whether or not this build answers it. What
/// stays enforced here is the other direction, that nothing is routed
/// outside the contract plus the declared local extensions, and that the
/// advertised set is the routed reference subset.
///
/// Which reference methods are still unrouted is a moving backlog, so it is
/// tracked where it is measured, in `app_server_surface_parity_tests`.
mod wire_tests;
mod worktree_tests;

/// Registers one tool whose runtime prerequisite never holds, which is what
/// a session-scoped tool does when the thing it drives is not there.
struct UnavailablePrerequisiteTools;

impl SessionToolFactory for UnavailablePrerequisiteTools {
    fn register(&self, _session_id: &str, tools: &ToolRegistry) -> Result<(), String> {
        tools
            .register_conditional(
                ToolSpec {
                    name: "fixture_probe".to_owned(),
                    description: "fixture".to_owned(),
                    input_schema: vibe_core::schema::ObjectSchema::new().build(),
                    output_schema: None,
                    config: Value::Null,
                    state: Value::Null,
                    availability: ToolAvailability::Available,
                    presentation: ToolPresentationKind::Generic,
                    source: ToolSource::BuiltIn,
                    selection_priority: 0,
                },
                Arc::new(
                    |_invocation: &ToolInvocation,
                     _output: ToolOutputSink|
                     -> OwnedToolHandlerFuture {
                        Box::pin(async { Ok(ToolExecutionOutput::text("unreachable")) })
                    },
                ),
                Arc::new(|| false),
            )
            .map(drop)
            .map_err(|error| error.to_string())
    }
}

struct RejectForkTools;

impl SessionToolFactory for RejectForkTools {
    fn register(&self, session_id: &str, _tools: &ToolRegistry) -> Result<(), String> {
        if session_id == "source-session" {
            Ok(())
        } else {
            Err("injected fork attachment failure".to_owned())
        }
    }
}

#[derive(Default)]
struct RecordingResourceBackend {
    opened_with_tools: Mutex<Option<usize>>,
    mcp_added: Mutex<bool>,
    closed: Mutex<Vec<String>>,
}

impl ResourceBackend for RecordingResourceBackend {
    fn open_session(&self, session: ResourceSession) -> Result<(), ResourceError> {
        let count = session
            .tools
            .list()
            .map_err(|error| ResourceError::Unavailable(error.to_string()))?
            .len();
        *self
            .opened_with_tools
            .lock()
            .map_err(|_| ResourceError::Unavailable("test backend lock".to_owned()))? = Some(count);
        Ok(())
    }

    fn dispatch<'a>(
        &'a self,
        request: ResourceBackendRequest,
    ) -> crate::resources::ResourceFuture<'a, ResourceDispatch> {
        Box::pin(async move {
            match request.command {
                ResourceBackendCommand::Mcp(crate::resources::McpCommand::Add(_)) => {
                    *self.mcp_added.lock().map_err(|_| {
                        ResourceError::Unavailable("test backend lock".to_owned())
                    })? = true;
                    Ok(ResourceDispatch {
                        result: result_map([("mcp", json!({"sources": ["example"]}))]),
                        signals: crate::resources::ResourceSignals {
                            runtime_updated: true,
                            ..crate::resources::ResourceSignals::default()
                        },
                    })
                }
                ResourceBackendCommand::Mcp(crate::resources::McpCommand::Read) => {
                    let added = *self
                        .mcp_added
                        .lock()
                        .map_err(|_| ResourceError::Unavailable("test backend lock".to_owned()))?;
                    Ok(ResourceDispatch {
                        result: result_map([(
                            "mcp",
                            json!({"sources": if added { vec!["example"] } else { vec![] }}),
                        )]),
                        signals: crate::resources::ResourceSignals::default(),
                    })
                }
                command => Err(ResourceError::MethodNotFound(format!("{command:?}"))),
            }
        })
    }

    fn close_session<'a>(
        &'a self,
        session_id: &'a str,
        _generation: u64,
    ) -> crate::resources::ResourceFuture<'a, ()> {
        Box::pin(async move {
            self.closed
                .lock()
                .map_err(|_| ResourceError::Unavailable("test backend lock".to_owned()))?
                .push(session_id.to_owned());
            Ok(())
        })
    }
}

fn request(id: i64, method: &str, params: Value) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    }))
    .expect("request fixture")
}

fn initialize(connection: &mut ServerConnection) {
    let batch = connection.dispatch(&request(
        1,
        "initialize",
        json!({
            "clientInfo": {
                "name": "test",
                "version": "1",
                "entrypoint": "programmatic",
                "terminalEmulator": "unknown"
            },
            "capabilities": {
                "callbackKinds": ["approval", "user_input"]
            }
        }),
    ));
    assert_eq!(batch.outbound.len(), 1);
    let initialized = serde_json::to_vec(&json!({
        "jsonrpc": "2.0",
        "method": "initialized",
        "params": {}
    }))
    .expect("initialized fixture");
    assert!(connection.dispatch(&initialized).outbound.is_empty());
    assert_eq!(connection.state(), ConnectionState::Ready);
}

/// Completes the handshake with the capabilities a client declares,
/// answering with the `InitializeResponse` the server sent.
fn initialize_with(connection: &mut ServerConnection, capabilities: Value) -> Value {
    let batch = connection.dispatch(&request(
        1,
        "initialize",
        json!({
            "clientInfo": {"name": "test", "version": "1"},
            "capabilities": capabilities
        }),
    ));
    let response = match decode_frame(&batch.outbound[0]).expect("handshake answer") {
        Envelope::Success(success) => Value::Object(success.result.into_iter().collect()),
        other => unreachable!("the handshake was rejected: {other:?}"),
    };
    let initialized = serde_json::to_vec(&json!({
        "jsonrpc": "2.0",
        "method": "initialized",
        "params": {}
    }))
    .expect("initialized fixture");
    assert!(connection.dispatch(&initialized).outbound.is_empty());
    response
}

/// Calls a session-scoped read and returns the result it answered with.
fn call(connection: &mut ServerConnection, id: i64, method: &str) -> BTreeMap<String, Value> {
    call_for(connection, id, method, "session-1")
}

fn call_for(
    connection: &mut ServerConnection,
    id: i64,
    method: &str,
    session_id: &str,
) -> BTreeMap<String, Value> {
    let batch = connection.dispatch(&request(id, method, json!({"sessionId": session_id})));
    match decode_frame(&batch.outbound[0]).expect("an answer") {
        Envelope::Success(SuccessResponse { result, .. }) => result,
        other => unreachable!("{method} did not answer: {other:?}"),
    }
}

fn start_session(connection: &mut ServerConnection) {
    let batch = connection.dispatch(&request(
        2,
        "session/start",
        json!({"sessionId": "session-1", "workingDirectory": "/workspace"}),
    ));
    // The answer, then the snapshot the attachment publishes.
    assert_eq!(batch.outbound.len(), 2);
    assert!(matches!(
        decode_frame(&batch.outbound[1]).expect("attachment frame"),
        Envelope::Notification(Notification { ref method, .. })
            if method == "session/snapshot"
    ));
}

fn message_entry(
    id: &str,
    session_id: &str,
    turn_id: &str,
    created_at: u64,
    text: &str,
) -> PublicHistoryEntry {
    PublicHistoryEntry::Message {
        metadata: PublicEntryMetadata {
            id: id.to_owned(),
            session_id: session_id.to_owned(),
            turn_id: Some(turn_id.to_owned()),
            created_at,
            updated_at: created_at,
            generation_status: PublicEntryGenerationStatus::Completed,
            related_entry_id: None,
        },
        role: PublicMessageRole::Assistant,
        content: vec![PublicContentBlock::Text {
            text: text.to_owned(),
        }],
        source: None,
        user_display_content: None,
    }
}
