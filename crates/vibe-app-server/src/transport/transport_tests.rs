use std::sync::atomic::{AtomicBool, Ordering};

use tokio::io::{BufReader, duplex};
use vibe_protocol::decode_frame;

use super::*;
use crate::client::EchoTurnDriver;
use crate::resources::{
    ResourceBackend, ResourceBackendRequest, ResourceDispatch, ResourceError, ResourceFuture,
    ResourceSession,
};
use crate::server::AppServer;

#[derive(Default)]
struct CleanupResourceBackend {
    opened: AtomicBool,
    closed: AtomicBool,
}

impl ResourceBackend for CleanupResourceBackend {
    fn open_session(&self, _session: ResourceSession) -> Result<(), ResourceError> {
        self.opened.store(true, Ordering::Release);
        Ok(())
    }

    fn dispatch<'a>(
        &'a self,
        _request: ResourceBackendRequest,
    ) -> ResourceFuture<'a, ResourceDispatch> {
        Box::pin(async move { Err(ResourceError::MethodNotFound("test".to_owned())) })
    }

    fn close_session<'a>(
        &'a self,
        _session_id: &'a str,
        _generation: u64,
    ) -> ResourceFuture<'a, ()> {
        Box::pin(async move {
            self.closed.store(true, Ordering::Release);
            Ok(())
        })
    }
}

#[tokio::test]
async fn stdio_transport_frames_json_by_newline_and_reports_eof() {
    let (mut client, server) = duplex(1024);
    let (server_read, server_write) = tokio::io::split(server);
    let mut transport = StdioTransport::new(BufReader::new(server_read), server_write);
    client
        .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"initialized\",\"params\":{}}\n")
        .await
        .expect("fixture write");
    let frame = transport
        .receive()
        .await
        .expect("frame read")
        .expect("not EOF");
    assert!(decode_frame(&frame).is_ok());
    drop(client);
    assert!(transport.receive().await.expect("EOF read").is_none());
}

#[tokio::test]
async fn stdio_transport_rejects_oversized_frames_before_decoding() {
    let input = vec![b'x'; MAX_FRAME_BYTES.saturating_add(1)];
    let reader = BufReader::new(std::io::Cursor::new(input));
    let writer = tokio::io::sink();
    let mut transport = StdioTransport::new(reader, writer);
    assert!(matches!(
        transport.receive().await,
        Err(TransportError::FrameTooLarge {
            limit: MAX_FRAME_BYTES
        })
    ));
}

#[tokio::test]
async fn stdio_server_flushes_turn_response_before_deferred_notification() {
    let (client, server_io) = duplex(4096);
    let (client_read, mut client_write) = tokio::io::split(client);
    let (server_read, server_write) = tokio::io::split(server_io);
    let server_task = tokio::spawn(serve_stdio(
        AppServer::default(),
        StdioTransport::new(BufReader::new(server_read), server_write),
        Arc::new(EchoTurnDriver::new("answer")),
    ));
    let mut responses = BufReader::new(client_read).lines();
    for request in [
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"clientInfo":{"name":"test","version":"1","entrypoint":"programmatic","terminalEmulator":"unknown"},"capabilities":{}}}"#,
        r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#,
        r#"{"jsonrpc":"2.0","id":2,"method":"session/start","params":{"sessionId":"session-1","workingDirectory":"/workspace"}}"#,
        r#"{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{"sessionId":"session-1","input":[{"type":"text","text":"hello"}]}}"#,
    ] {
        client_write
            .write_all(request.as_bytes())
            .await
            .expect("request bytes");
        client_write
            .write_all(b"\n")
            .await
            .expect("request newline");
    }
    // Responses and notifications share one stream, so the frames are read
    // as they arrive and separated by whether they name a method.
    let mut responses_seen = Vec::new();
    let mut notifications = Vec::new();
    loop {
        let frame = responses
            .next_line()
            .await
            .expect("frame read")
            .expect("frame");
        let frame = serde_json::from_str::<serde_json::Value>(&frame).expect("frame JSON");
        let completed = frame["method"] == "turn/completed";
        if frame["method"].is_string() {
            notifications.push(frame);
        } else {
            responses_seen.push(frame);
        }
        assert!(
            responses_seen.len() + notifications.len() <= 16,
            "the turn emitted too many frames"
        );
        if completed {
            break;
        }
    }
    assert_eq!(
        responses_seen
            .iter()
            .map(|response| response["id"].as_u64())
            .collect::<Vec<_>>(),
        vec![Some(1), Some(2), Some(3)]
    );
    assert_eq!(
        notifications
            .iter()
            .map(|notification| notification["method"].as_str().unwrap_or_default())
            .collect::<Vec<_>>(),
        vec![
            "session/snapshot",
            "turn/started",
            "session/updated",
            "history/entryAdded",
            "history/entryAdded",
            "history/entryUpdated",
            "session/statsUpdated",
            "session/updated",
            "turn/completed",
        ]
    );
    // The client's own projection raises on a gap, so the sequence has to
    // run from one without one.
    assert_eq!(
        notifications
            .iter()
            .map(|notification| notification["params"]["eventId"].as_u64())
            .collect::<Vec<_>>(),
        (1..=9).map(Some).collect::<Vec<_>>()
    );
    // The snapshot names its own watermark, which is what the reference
    // projection asserts before it adopts the state.
    assert_eq!(
        notifications[0]["params"]["state"]["eventId"],
        notifications[0]["params"]["eventId"]
    );
    assert_eq!(
        notifications[2]["params"]["patch"][0]["value"]["type"],
        "running"
    );
    assert_eq!(
        notifications[7]["params"]["patch"][0]["value"]["type"],
        "idle"
    );
    // The settled turn publishes its accounting before its status.
    let stats = &notifications[6]["params"];
    assert_eq!(stats["stats"]["steps"], 1);
    assert!(stats["contextWindow"].is_u64());
    let assistant = &notifications[4]["params"]["entry"];
    assert_eq!(assistant["role"], "assistant");
    assert_eq!(assistant["generationStatus"], "in_progress");
    let completion_patch = notifications[5]["params"]["patch"]
        .as_array()
        .expect("completion patch");
    assert!(completion_patch.iter().any(|operation| {
        operation["op"] == "replace"
            && operation["path"] == "/generationStatus"
            && operation["value"] == "completed"
    }));
    drop(client_write);
    drop(responses);
    server_task
        .await
        .expect("server task joins")
        .expect("server exits cleanly");
}

/// A connection the server gives up on says so before it goes quiet, so a
/// client learns why the stream stopped rather than only that it did.
#[tokio::test]
async fn a_fatal_transport_failure_publishes_an_error_before_the_stream_ends() {
    let (client, server_io) = duplex(4096);
    let (client_read, mut client_write) = tokio::io::split(client);
    let (server_read, server_write) = tokio::io::split(server_io);
    let server_task = tokio::spawn(serve_stdio(
        AppServer::default(),
        StdioTransport::new(BufReader::new(server_read), server_write),
        Arc::new(EchoTurnDriver::new("answer")),
    ));
    let mut responses = BufReader::new(client_read).lines();
    for request in [
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"clientInfo":{"name":"test","version":"1","entrypoint":"programmatic","terminalEmulator":"unknown"},"capabilities":{}}}"#,
        r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#,
        // An empty frame is unreadable rather than merely invalid, which is
        // what the transport reports as fatal.
        "",
    ] {
        client_write
            .write_all(request.as_bytes())
            .await
            .expect("request bytes");
        client_write
            .write_all(b"\n")
            .await
            .expect("request newline");
    }
    assert!(responses.next_line().await.expect("initialize").is_some());
    let frame = responses
        .next_line()
        .await
        .expect("error read")
        .expect("the failure is published");
    let frame = serde_json::from_str::<serde_json::Value>(&frame).expect("error JSON");
    assert_eq!(frame["method"], "error");
    assert!(
        frame["params"]["error"]["message"]
            .as_str()
            .is_some_and(|message| !message.is_empty()),
        "the failure names itself: {frame}"
    );
    assert!(frame["params"]["error"]["code"].is_null());
    assert!(frame["params"]["error"]["details"].is_null());
    // The stream stops right after, which is the point of sending it.
    assert!(responses.next_line().await.expect("stream end").is_none());
    drop(client_write);
    drop(responses);
    assert!(
        server_task.await.expect("server task joins").is_err(),
        "the connection ends on the failure it published"
    );
}

#[tokio::test]
async fn stdio_transport_loss_closes_orphaned_resource_sessions() {
    let backend = Arc::new(CleanupResourceBackend::default());
    let (client, server_io) = duplex(4096);
    let (client_read, mut client_write) = tokio::io::split(client);
    let (server_read, server_write) = tokio::io::split(server_io);
    let server_task = tokio::spawn(serve_stdio(
        AppServer::with_resource_backend(backend.clone()),
        StdioTransport::new(BufReader::new(server_read), server_write),
        Arc::new(EchoTurnDriver::new("answer")),
    ));
    let mut responses = BufReader::new(client_read).lines();
    for request in [
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"clientInfo":{"name":"test","version":"1","entrypoint":"programmatic","terminalEmulator":"unknown"},"capabilities":{}}}"#,
        r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#,
        r#"{"jsonrpc":"2.0","id":2,"method":"session/start","params":{"sessionId":"session-1","workingDirectory":"/workspace"}}"#,
    ] {
        client_write
            .write_all(request.as_bytes())
            .await
            .expect("request bytes");
        client_write
            .write_all(b"\n")
            .await
            .expect("request newline");
    }
    assert!(responses.next_line().await.expect("initialize").is_some());
    assert!(responses.next_line().await.expect("session").is_some());
    assert!(backend.opened.load(Ordering::Acquire));
    drop(client_write);
    drop(responses);
    server_task
        .await
        .expect("server task")
        .expect("transport closes");
    assert!(backend.closed.load(Ordering::Acquire));
}

/// The delegation reaches the client over the same stream its requests
/// arrive on, and the answer travels back to the tool that is waiting.
///
/// The tool is invoked on the server handle the serve loop is holding, which
/// is how a turn running off the read loop raises a `clientTool/*` request
/// in production.
#[tokio::test]
async fn stdio_server_carries_a_client_tool_delegation_and_its_answer() {
    let directory = tempfile::tempdir().expect("tempdir");
    std::fs::write(directory.path().join("main.rs"), "on disk\n").expect("file");
    let server = AppServer::default();
    let (client, server_io) = duplex(4096);
    let (client_read, mut client_write) = tokio::io::split(client);
    let (server_read, server_write) = tokio::io::split(server_io);
    let server_task = tokio::spawn(serve_stdio(
        server.clone(),
        StdioTransport::new(BufReader::new(server_read), server_write),
        Arc::new(EchoTurnDriver::new("answer")),
    ));
    let mut frames = BufReader::new(client_read).lines();
    for request in [
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"clientInfo":{"name":"editor","version":"1"},"capabilities":{"clientTools":["filesystem/read"]}}}"#.to_owned(),
            r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#.to_owned(),
            format!(
                r#"{{"jsonrpc":"2.0","id":2,"method":"session/start","params":{{"sessionId":"session-1","workingDirectory":{},"trusted":true,"autoApprove":true}}}}"#,
                serde_json::Value::String(directory.path().to_string_lossy().into_owned())
            ),
        ] {
            client_write
                .write_all(request.as_bytes())
                .await
                .expect("request bytes");
            client_write
                .write_all(b"\n")
                .await
                .expect("request newline");
        }
    assert!(frames.next_line().await.expect("initialize").is_some());
    assert!(frames.next_line().await.expect("session").is_some());

    let reader = server.clone();
    let call = tokio::spawn(async move {
        reader
            .invoke_tool(
                "session-1",
                "read_file",
                vibe_core::tools::ToolInvocation {
                    call_id: "read-1".to_owned(),
                    arguments: serde_json::json!({"file_path": "main.rs"}),
                },
            )
            .await
    });

    // Attaching the session put its snapshot on the same stream, so the
    // delegation is picked out by being a request rather than by position.
    let delegated = loop {
        let frame = frames
            .next_line()
            .await
            .expect("delegated frame")
            .expect("the delegation reaches the wire");
        let frame = serde_json::from_str::<serde_json::Value>(&frame).expect("delegated JSON");
        if frame["method"].is_string() && !frame["id"].is_null() {
            break frame;
        }
    };
    assert_eq!(delegated["method"], "clientTool/readTextFile");
    assert_eq!(delegated["params"]["sessionId"], "session-1");
    assert_eq!(
        delegated["params"]["path"],
        serde_json::Value::String(
            std::fs::canonicalize(directory.path())
                .expect("canonical root")
                .join("main.rs")
                .to_string_lossy()
                .into_owned()
        )
    );

    let answer = serde_json::json!({
        "jsonrpc": "2.0",
        "id": delegated["id"],
        "result": {"content": "unsaved\n"},
    });
    client_write
        .write_all(serde_json::to_string(&answer).expect("answer").as_bytes())
        .await
        .expect("answer bytes");
    client_write.write_all(b"\n").await.expect("answer newline");

    let output = call.await.expect("the tool task joins").expect("the read");
    assert_eq!(output.typed_result["content"], "        1\u{2192}unsaved");

    drop(client_write);
    drop(frames);
    server_task
        .await
        .expect("server task")
        .expect("transport closes");
}
