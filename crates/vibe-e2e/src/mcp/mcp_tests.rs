use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use serde_json::{Value, json};

use super::{Answer, Calls, Mcp, answer, text_result};
use crate::Platform;

fn script() -> Value {
    Mcp::new()
        .tool(json!({"name": "echo", "inputSchema": {"type": "object"}}))
        .react("echo", text_result("first"))
        .react(
            "echo",
            json!({"error": {"code": -32000, "message": "second"}}),
        )
        .react("echo", json!({"crash": true}))
        .to_json()
}

fn responded(answer: Answer) -> Option<Value> {
    match answer {
        Answer::Respond(response, _) => Some(response),
        Answer::Nothing | Answer::Crash => None,
    }
}

fn call(id: u64) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": "echo"}})
}

#[test]
fn successive_calls_take_successive_reactions_and_the_last_repeats() {
    let script = script();
    let mut calls = Calls::default();
    let first =
        responded(answer(&script, &mut calls, &call(1))).expect("the first call was not answered");
    assert_eq!(first["result"]["content"][0]["text"], "first");
    let second =
        responded(answer(&script, &mut calls, &call(2))).expect("the second call was not answered");
    assert_eq!(second["error"]["code"], -32000);
    assert_eq!(answer(&script, &mut calls, &call(3)), Answer::Crash);
    assert_eq!(answer(&script, &mut calls, &call(4)), Answer::Crash);
}

#[test]
fn initialize_echoes_the_version_and_lists_the_tools() {
    let script = script();
    let mut calls = Calls::default();
    let initialize = json!({"jsonrpc": "2.0", "id": 0, "method": "initialize",
                            "params": {"protocolVersion": "2025-03-26"}});
    let response =
        responded(answer(&script, &mut calls, &initialize)).expect("initialize was not answered");
    assert_eq!(response["result"]["protocolVersion"], "2025-03-26");
    let list = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"});
    let response =
        responded(answer(&script, &mut calls, &list)).expect("tools/list was not answered");
    assert_eq!(response["result"]["tools"][0]["name"], "echo");
    let notification = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
    assert_eq!(answer(&script, &mut calls, &notification), Answer::Nothing);
}

/// Posts `body` to `path` on the platform and returns the status and body.
fn post_path(platform: &Platform, path: &str, body: &Value) -> (u16, String) {
    let mut stream = TcpStream::connect(platform.address()).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let body = body.to_string();
    write!(
        stream,
        "POST {path} HTTP/1.1\r\nhost: e2e\r\ncontent-type: application/json\r\n\
         content-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut response = String::new();
    let _ = BufReader::new(stream).read_to_string(&mut response);
    let status = response
        .split(' ')
        .nth(1)
        .unwrap_or("0")
        .parse()
        .unwrap_or(0);
    (status, response)
}

#[test]
fn a_declared_connector_gateway_answers_as_an_mcp_server() {
    let platform = Platform::start().unwrap();
    platform.gateway(
        "c-drive",
        &Mcp::new().react("echo", text_result("found it")),
    );
    let (status, body) = post_path(&platform, "/v1/connectors-gateway/c-drive/mcp", &call(1));
    assert_eq!(status, 200);
    assert!(body.contains("found it"), "{body}");
    let (status, _) = post_path(&platform, "/v1/connectors-gateway/c-other/mcp", &call(2));
    assert_eq!(
        status, 404,
        "an undeclared gateway keeps its neutral answer"
    );
    assert_eq!(platform.mcp_requests("gateway-c-drive").len(), 1);
    assert!(platform.failures().is_empty(), "{:?}", platform.failures());
}

/// Posts `body` to the platform's MCP server `name` and returns the status,
/// the session header, and the body.
fn post(
    platform: &Platform,
    name: &str,
    body: &Value,
    session: Option<&str>,
) -> (u16, Option<String>, String) {
    let mut stream = TcpStream::connect(platform.address()).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let body = body.to_string();
    let session = session.map_or(String::new(), |id| format!("mcp-session-id: {id}\r\n"));
    write!(
        stream,
        "POST /mcp/{name} HTTP/1.1\r\nhost: e2e\r\ncontent-type: application/json\r\n{session}\
         content-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut reader = BufReader::new(stream);
    let mut status_line = String::new();
    if reader.read_line(&mut status_line).unwrap_or(0) == 0 {
        return (0, None, String::new());
    }
    let status = status_line.split(' ').nth(1).unwrap().parse().unwrap();
    let mut header = None;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        if line.trim().is_empty() {
            break;
        }
        if let Some(value) = line.to_ascii_lowercase().strip_prefix("mcp-session-id:") {
            header = Some(value.trim().to_owned());
        }
    }
    let mut rest = String::new();
    let _ = reader.read_to_string(&mut rest);
    (status, header, rest)
}

#[test]
fn a_crash_drops_the_connection_and_ends_the_session() {
    let platform = Platform::start().unwrap();
    platform.mcp(
        "srv",
        &Mcp::new()
            .react("echo", json!({"crash": true}))
            .react("echo", text_result("back")),
    );
    let initialize = json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {}});
    let (status, session, _) = post(&platform, "srv", &initialize, None);
    assert_eq!(status, 200);
    let session = session.expect("initialize sets a session id");
    let (status, _, _) = post(&platform, "srv", &call(1), Some(&session));
    assert_eq!(status, 0, "the crashing call was answered");
    let (status, _, _) = post(&platform, "srv", &call(2), Some(&session));
    assert_eq!(status, 404, "the session survived the crash");
    let (_, fresh, _) = post(&platform, "srv", &initialize, None);
    let (status, _, body) = post(&platform, "srv", &call(3), fresh.as_deref());
    assert_eq!(status, 200);
    assert!(body.contains("back"), "{body}");
    let queues: Vec<String> = platform
        .mcp_requests("srv")
        .into_iter()
        .map(|e| e.queue)
        .collect();
    assert_eq!(queues.len(), 5);
    assert!(platform.failures().is_empty(), "{:?}", platform.failures());
}
