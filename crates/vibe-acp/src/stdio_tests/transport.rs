//! Transport-level behavior: the writer queue, the client port, and the
//! frames the read loop refuses without stopping.

use std::sync::Arc;

use serde_json::json;
use tokio::sync::{mpsc, oneshot};
use vibe_app_server::client::EchoTurnDriver;

use super::{initialize, new_session, request, spawn_stdio};
use crate::stdio::client::{
    MAX_PENDING_CLIENT_REQUESTS, StdioClientPort, WriterMessage, send_value,
};
use vibe_acp::AcpClientPort;

#[test]
fn writer_queue_reports_saturation_without_allocating_past_its_bound() {
    let (writer, _receiver) = mpsc::channel(1);
    send_value(&writer, json!({"first": true})).expect("first value fits");
    let error =
        send_value(&writer, json!({"second": true})).expect_err("second value exceeds the bound");
    assert!(error.contains("no available capacity"), "{error}");
}

#[tokio::test]
async fn client_request_limit_and_disconnect_fail_closed() {
    let (writer, mut receiver) = mpsc::channel(1);
    let client = Arc::new(StdioClientPort::new(writer));
    {
        let mut pending = client.pending.lock().expect("pending lock");
        for id in 0..MAX_PENDING_CLIENT_REQUESTS {
            let (sender, _receiver) = oneshot::channel();
            pending.insert(i64::try_from(id).expect("bounded ID"), sender);
        }
    }
    let error = client
        .request("session/request_permission", json!({}))
        .await
        .expect_err("pending request limit is enforced");
    assert!(error.contains("too many pending"), "{error}");

    client.pending.lock().expect("pending lock").clear();
    let request_client = client.clone();
    let request = tokio::spawn(async move {
        request_client
            .request("session/request_permission", json!({}))
            .await
    });
    assert!(matches!(
        receiver.recv().await,
        Some(WriterMessage::Value(_))
    ));
    client.disconnect();
    let error = request
        .await
        .expect("request task")
        .expect_err("disconnect rejects pending request");
    assert!(error.contains("disconnected"), "{error}");
    assert!(client.pending.lock().expect("pending lock").is_empty());
}

/// The reference's router hands every extension notification to
/// `ext_notification`, which serves `telemetry/send` and returns on any other
/// name, and its connection answers a notification with nothing. So none of
/// these three frames puts anything on the wire, including the one whose
/// payload the model refuses.
#[tokio::test]
async fn extension_notifications_are_served_without_answering_the_wire() {
    let mut peer = spawn_stdio(EchoTurnDriver::new("answer"));
    initialize(&mut peer, 1).await;
    let session_id = new_session(&mut peer, 2, "/workspace").await;

    for notification in [
        json!({
            "jsonrpc": "2.0",
            "method": "_telemetry/send",
            "params": {
                "event": "vibe.at_mention_inserted",
                "properties": {"mention_type": "file"},
                "sessionId": session_id,
            },
        }),
        json!({
            "jsonrpc": "2.0",
            "method": "_telemetry/send",
            "params": {"event": "vibe.at_mention_inserted"},
        }),
        json!({"jsonrpc": "2.0", "method": "_editor/heartbeat", "params": {}}),
    ] {
        peer.send(notification).await;
    }

    peer.send(request(3, "session/list", json!({}))).await;
    let (preceding, response) = peer.response(3).await;
    assert!(
        preceding.is_empty(),
        "an extension notification was answered: {preceding:?}"
    );
    assert!(response.get("error").is_none(), "{response}");
    peer.shutdown(4).await;
}

#[tokio::test]
async fn malformed_frames_return_protocol_errors_without_stopping_the_loop() {
    let mut peer = spawn_stdio(EchoTurnDriver::new("answer"));

    peer.send_raw(b"{not-json").await;
    let parse_error = peer.next().await;
    assert!(parse_error["id"].is_null());
    assert_eq!(parse_error["error"]["code"], -32700);

    peer.send_raw(br#"{"jsonrpc":"2.0","id":2,"method":7}"#)
        .await;
    let request_error = peer.next().await;
    assert!(request_error["id"].is_null());
    assert_eq!(request_error["error"]["code"], -32600);

    let initialized = initialize(&mut peer, 3).await;
    assert_eq!(
        initialized["result"]["protocolVersion"],
        vibe_acp::ACP_PROTOCOL_VERSION
    );
    peer.shutdown(4).await;
}
