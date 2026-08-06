//! The delegation from a real connection to a real tool call.
//!
//! Every case drives the whole path a client tool takes: a handshake that
//! declares a capability, a session whose tools are published against it, a tool
//! invoked through the server, the request frame that leaves on the connection's
//! write side, and the client answer routed back through `dispatch`. A test
//! that called the bridge directly would prove the transport and none of the
//! wiring that makes a tool reach it.

use std::path::Path;

use serde_json::{Value, json};
use tokio::sync::mpsc;

use vibe_core::tools::ToolInvocation;
use vibe_protocol::{Envelope, RequestId, TransportKind, decode_frame, encode_frame};

use crate::server::{AppServer, ServerConnection};

const SESSION: &str = "session-1";

fn frame(id: i64, method: &str, params: &Value) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    }))
    .expect("request frame")
}

/// A connection that declared `client_tools`, with one session open on `root`
/// and the write side of its delegation on the returned receiver.
fn connected(
    root: &Path,
    client_tools: &[&str],
) -> (
    AppServer,
    ServerConnection,
    mpsc::UnboundedReceiver<Vec<u8>>,
) {
    let server = AppServer::default();
    let mut connection = server.connect(TransportKind::InProcess);
    let (sender, receiver) = mpsc::unbounded_channel();
    connection.client_tools().attach(sender);
    let handshake = connection.dispatch(&frame(
        1,
        "initialize",
        &json!({
            "clientInfo": {"name": "editor", "version": "1"},
            "capabilities": {
                "callbackKinds": ["approval"],
                "clientTools": client_tools,
                "disabledNotifications": []
            }
        }),
    ));
    assert!(
        matches!(
            decode_frame(&handshake.outbound[0]).expect("handshake answer"),
            Envelope::Success(_)
        ),
        "the handshake declaring {client_tools:?} was rejected"
    );
    connection.dispatch(
        &serde_json::to_vec(&json!({"jsonrpc": "2.0", "method": "initialized", "params": {}}))
            .expect("initialized frame"),
    );
    let started = connection.dispatch(&frame(
        2,
        "session/start",
        &json!({
            "sessionId": SESSION,
            "workingDirectory": root.to_string_lossy(),
            "trusted": true,
            "autoApprove": true,
        }),
    ));
    assert!(
        matches!(
            decode_frame(&started.outbound[0]).expect("session answer"),
            Envelope::Success(_)
        ),
        "the session did not start"
    );
    (server, connection, receiver)
}

/// The next delegated request on the wire, as its id, method and params.
async fn delegated(requests: &mut mpsc::UnboundedReceiver<Vec<u8>>) -> (RequestId, String, Value) {
    let bytes = tokio::time::timeout(std::time::Duration::from_secs(5), requests.recv())
        .await
        .expect("a delegated request reaches the wire")
        .expect("the delegation channel is open");
    let Envelope::Request(request) = decode_frame(&bytes).expect("delegated frame") else {
        unreachable!("a delegation is a server-to-client request: {bytes:?}");
    };
    (
        request.id,
        request.method,
        Value::Object(request.params.into_iter().collect()),
    )
}

fn answer(id: RequestId, result: Value) -> Vec<u8> {
    encode_frame(&Envelope::Success(vibe_protocol::SuccessResponse {
        jsonrpc: vibe_protocol::JsonRpcVersion::V2,
        id,
        result: result
            .as_object()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .collect(),
    }))
}

/// The whole round trip: the agent reads a file, the request leaves as
/// `clientTool/readTextFile`, and the answer is what the tool reports.
#[tokio::test]
async fn a_declared_filesystem_read_reaches_the_client_and_comes_back() {
    let directory = tempfile::tempdir().expect("tempdir");
    std::fs::write(directory.path().join("main.rs"), "on disk\n").expect("file");
    let (server, mut connection, mut requests) = connected(directory.path(), &["filesystem/read"]);

    let reader = server.clone();
    let call = tokio::spawn(async move {
        reader
            .invoke_tool(
                SESSION,
                "read_file",
                ToolInvocation {
                    call_id: "read-1".to_owned(),
                    arguments: json!({"file_path": "main.rs"}),
                },
            )
            .await
    });

    let (id, method, params) = delegated(&mut requests).await;
    assert_eq!(method, "clientTool/readTextFile");
    assert_eq!(params["sessionId"], json!(SESSION));
    // The confined absolute path, which is the only form the client can
    // resolve: the request carries no working directory.
    assert_eq!(
        params["path"],
        json!(
            std::fs::canonicalize(directory.path())
                .expect("canonical root")
                .join("main.rs")
                .to_string_lossy()
        )
    );
    assert!(params["line"].is_null(), "{params}");
    assert!(params["limit"].is_i64(), "{params}");

    let batch = connection.dispatch(&answer(id, json!({"content": "unsaved\n"})));
    assert!(batch.outbound.is_empty(), "an answered delegation replied");
    assert!(
        !batch.close_after_flush,
        "an answered delegation closed the connection"
    );

    let output = call.await.expect("the tool task joins").expect("the read");
    assert_eq!(output.model_text, "1|unsaved");
}

/// A rejected delegation is a tool failure carrying what the client said, not a
/// turn that hangs on an answer that will never come.
#[tokio::test]
async fn a_client_that_rejects_a_delegation_fails_the_tool() {
    let directory = tempfile::tempdir().expect("tempdir");
    std::fs::write(directory.path().join("main.rs"), "on disk\n").expect("file");
    let (server, mut connection, mut requests) = connected(directory.path(), &["filesystem/read"]);

    let reader = server.clone();
    let call = tokio::spawn(async move {
        reader
            .invoke_tool(
                SESSION,
                "read_file",
                ToolInvocation {
                    call_id: "read-1".to_owned(),
                    arguments: json!({"file_path": "main.rs"}),
                },
            )
            .await
    });

    let (id, _, _) = delegated(&mut requests).await;
    let rejection = encode_frame(&Envelope::Error(vibe_protocol::ErrorResponse {
        jsonrpc: vibe_protocol::JsonRpcVersion::V2,
        id,
        error: vibe_protocol::ProtocolError {
            code: vibe_protocol::ProtocolErrorCode::InternalError,
            message: "the editor has no buffer for that path".to_owned(),
            data: Value::Null,
        },
    }));
    let batch = connection.dispatch(&rejection);
    assert!(
        !batch.close_after_flush,
        "a rejected delegation closed the connection"
    );

    let failure = call
        .await
        .expect("the tool task joins")
        .expect_err("a rejected delegation is a failure");
    assert!(
        failure.to_string().contains("no buffer for that path"),
        "the failure does not carry what the client said: {failure}"
    );
}

/// A client that declared nothing is never asked, so the tools answer from this
/// host and no frame is issued.
#[tokio::test]
async fn a_client_declaring_nothing_is_never_asked() {
    let directory = tempfile::tempdir().expect("tempdir");
    std::fs::write(directory.path().join("main.rs"), "on disk\n").expect("file");
    let (server, _connection, mut requests) = connected(directory.path(), &[]);

    let output = server
        .invoke_tool(
            SESSION,
            "read_file",
            ToolInvocation {
                call_id: "read-1".to_owned(),
                arguments: json!({"file_path": "main.rs"}),
            },
        )
        .await
        .expect("this host answers the read");
    assert_eq!(output.model_text, "1|on disk");
    assert!(
        requests.try_recv().is_err(),
        "an undeclared capability still reached the client"
    );
}

/// Closing the connection releases what is parked on it rather than leaving a
/// tool to wait out its deadline on a client that is gone.
#[tokio::test]
async fn closing_the_connection_fails_the_delegations_it_was_carrying() {
    let directory = tempfile::tempdir().expect("tempdir");
    std::fs::write(directory.path().join("main.rs"), "on disk\n").expect("file");
    let (server, mut connection, mut requests) = connected(directory.path(), &["filesystem/read"]);

    let reader = server.clone();
    let call = tokio::spawn(async move {
        reader
            .invoke_tool(
                SESSION,
                "read_file",
                ToolInvocation {
                    call_id: "read-1".to_owned(),
                    arguments: json!({"file_path": "main.rs"}),
                },
            )
            .await
    });
    delegated(&mut requests).await;
    connection.close();

    let failure = call
        .await
        .expect("the tool task joins")
        .expect_err("a dropped client is a failure");
    assert!(
        failure.to_string().contains("disconnected"),
        "the failure does not name the disconnection: {failure}"
    );
}

/// A closing session releases the terminal its client still holds, so a
/// delegation that was abandoned mid-command does not outlive the session.
#[tokio::test]
async fn closing_a_session_releases_the_terminal_its_client_still_holds() {
    let directory = tempfile::tempdir().expect("tempdir");
    let (server, mut connection, mut requests) = connected(directory.path(), &["terminal"]);

    let runner = server.clone();
    let call = tokio::spawn(async move {
        runner
            .invoke_tool(
                SESSION,
                "bash",
                ToolInvocation {
                    call_id: "bash-1".to_owned(),
                    arguments: json!({"command": "sleep 60"}),
                },
            )
            .await
    });

    let (create, method, _) = delegated(&mut requests).await;
    assert_eq!(method, "clientTool/terminal/create");
    connection.dispatch(&answer(create, json!({"terminalId": "editor-terminal-1"})));
    let (_, waiting, _) = delegated(&mut requests).await;
    assert_eq!(waiting, "clientTool/terminal/wait");

    // The wait is never answered, so the terminal is still open when the
    // session closes. The close runs alongside the reader because it waits out
    // the acknowledgements this client will not send.
    let closing = tokio::spawn(async move { server.close_resource_session(SESSION, 0).await });
    let mut issued = Vec::new();
    while !issued
        .iter()
        .any(|method| method == "clientTool/terminal/release")
    {
        let (_, method, _) = delegated(&mut requests).await;
        issued.push(method);
    }
    assert_eq!(
        issued,
        ["clientTool/terminal/kill", "clientTool/terminal/release"],
        "a closing session did not kill the terminal before releasing it"
    );
    call.abort();
    closing.abort();
}

/// An in-process connection has no write side, which is what the ACP bridge is:
/// it declares the capabilities its editor answers through its own `acp_*` tool
/// family, and no `clientTool/*` frame can leave a connection nothing writes
/// to. The delegation stays off there, or every file read and every command in
/// an ACP session fails on a request that reaches no one.
#[tokio::test]
async fn a_connection_with_no_write_side_leaves_the_tools_on_this_host() {
    let directory = tempfile::tempdir().expect("tempdir");
    std::fs::write(directory.path().join("main.rs"), "on disk\n").expect("file");
    let server = AppServer::default();
    let mut connection = server.connect(TransportKind::InProcess);
    connection.dispatch(&frame(
        1,
        "initialize",
        &json!({
            "clientInfo": {"name": "acp", "version": "1"},
            "capabilities": {"clientTools": ["filesystem/read", "filesystem/write", "terminal"]}
        }),
    ));
    connection.dispatch(
        &serde_json::to_vec(&json!({"jsonrpc": "2.0", "method": "initialized", "params": {}}))
            .expect("initialized frame"),
    );
    connection.dispatch(&frame(
        2,
        "session/start",
        &json!({
            "sessionId": SESSION,
            "workingDirectory": directory.path().to_string_lossy(),
            "trusted": true,
            "autoApprove": true,
        }),
    ));

    let output = server
        .invoke_tool(
            SESSION,
            "read_file",
            ToolInvocation {
                call_id: "read-1".to_owned(),
                arguments: json!({"file_path": "main.rs"}),
            },
        )
        .await
        .expect("this host answers the read");
    assert_eq!(output.model_text, "1|on disk");
}

/// Hosting the filesystem is not a way around the workspace boundary: a path
/// the local tools refuse never reaches the client either.
#[tokio::test]
async fn a_path_outside_the_workspace_is_refused_before_it_reaches_the_client() {
    let directory = tempfile::tempdir().expect("tempdir");
    let root = directory.path().join("workspace");
    std::fs::create_dir(&root).expect("workspace root");
    let outside = directory.path().join("outside.txt");
    std::fs::write(&outside, "secret\n").expect("outside file");
    let (server, _connection, mut requests) =
        connected(&root, &["filesystem/read", "filesystem/write", "terminal"]);

    for (tool, arguments) in [
        ("read_file", json!({"file_path": outside.to_string_lossy()})),
        (
            "write_file",
            json!({"file_path": "../outside.txt", "content": "overwritten\n"}),
        ),
        (
            "edit",
            json!({
                "file_path": outside.to_string_lossy(),
                "old_string": "secret",
                "new_string": "leaked",
            }),
        ),
    ] {
        let failure = server
            .invoke_tool(
                SESSION,
                tool,
                ToolInvocation {
                    call_id: format!("{tool}-1"),
                    arguments,
                },
            )
            .await
            .expect_err("a path outside the workspace is refused");
        assert!(
            failure.to_string().contains("escapes the authorized root"),
            "{tool} did not refuse the escape: {failure}"
        );
    }
    assert!(
        requests.try_recv().is_err(),
        "a path outside the workspace reached the client"
    );
    assert_eq!(
        std::fs::read_to_string(&outside).expect("outside file"),
        "secret\n"
    );
}
