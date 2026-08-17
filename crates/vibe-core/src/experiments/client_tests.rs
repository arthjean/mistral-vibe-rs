//! The lookup, its payload and the four ways it can produce nothing.

use std::io::{Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;

use crate::observability::LogLevel;

use super::client::{EvalFailure, EvalPayload, RemoteEvalClient};
use super::models::ExperimentAttributes;
use super::recorder::{Outcome, RecordingTransport};
use super::{EVAL_REQUEST_TIMEOUT, EVAL_REQUEST_TIMEOUT_SECONDS, build_eval_url};

const ORACLE_URL: &str = "https://experiments.example.test/api/eval/sdk-key";

fn attributes() -> ExperimentAttributes {
    ExperimentAttributes {
        user_id: "0123456789abcdef0123456789abcdef".to_owned(),
        entrypoint: "cli".to_owned(),
        agent_version: "9.9.9".to_owned(),
        client_name: Some("oracle-client".to_owned()),
        client_version: Some("1.2.3".to_owned()),
        os: "linux".to_owned(),
        terminal_emulator: Some("vscode".to_owned()),
        custom_system_prompt: false,
        organization_id: Some("oracle-organization".to_owned()),
    }
}

fn client_over(outcome: Outcome) -> (RemoteEvalClient, Arc<RecordingTransport>) {
    let transport = Arc::new(RecordingTransport::new(outcome));
    let client =
        RemoteEvalClient::with_transport(Some(ORACLE_URL.to_owned()), Arc::clone(&transport) as _);
    (client, transport)
}

/// The timers and the I/O driver are enabled because the production transport
/// carries a timeout, which `reqwest` arms against the runtime's own clock.
fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a current-thread runtime builds")
}

#[test]
fn the_url_is_the_host_and_the_key_with_one_path_between_them() {
    let client = RemoteEvalClient::from_settings("https://experiments.example.test/", " sdk-key ");
    assert_eq!(client.url(), Some(ORACLE_URL));
    assert_eq!(
        build_eval_url("https://experiments.example.test", "sdk-key").as_deref(),
        Some(ORACLE_URL)
    );
}

#[test]
fn an_empty_host_or_key_leaves_the_client_with_no_url_and_issues_nothing() {
    for (host, key) in [
        ("", "sdk-key"),
        ("   ", "sdk-key"),
        ("///", "sdk-key"),
        ("https://experiments.example.test", ""),
        ("https://experiments.example.test", "  "),
    ] {
        let client = RemoteEvalClient::from_settings(host, key);
        assert_eq!(client.url(), None, "{host:?} with {key:?} resolves no URL");
        let answered = runtime().block_on(client.evaluate(&attributes()));
        assert!(answered.is_none());
    }

    // And with a transport standing by, an unset URL still sends nothing.
    let transport = Arc::new(RecordingTransport::answering(r#"{"features": {}}"#));
    let client = RemoteEvalClient::with_transport(None, Arc::clone(&transport) as _);
    assert!(runtime().block_on(client.evaluate(&attributes())).is_none());
    assert_eq!(transport.request_count(), 0);
}

#[test]
fn the_posted_body_carries_the_attributes_and_the_three_empty_defaults() {
    let (client, transport) = client_over(Outcome::ok(r#"{"features": {}}"#));
    let answered = runtime().block_on(client.evaluate(&attributes()));
    assert!(answered.is_some(), "a 200 with a valid body resolves");

    let requests = transport.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.method, "POST");
    assert_eq!(request.url, ORACLE_URL);
    assert!(
        request.header_names.is_empty(),
        "the client sets no header of its own"
    );
    let payload = request
        .payload
        .as_object()
        .expect("the payload is an object");
    let mut keys = payload.keys().map(String::as_str).collect::<Vec<_>>();
    keys.sort_unstable();
    assert_eq!(
        keys,
        ["attributes", "forcedFeatures", "forcedVariations", "url"]
    );
    assert_eq!(
        payload.get("forcedVariations"),
        Some(&serde_json::json!({}))
    );
    assert_eq!(payload.get("forcedFeatures"), Some(&serde_json::json!([])));
    assert_eq!(payload.get("url"), Some(&serde_json::json!("")));
    assert_eq!(
        payload
            .get("attributes")
            .and_then(|attributes| attributes.get("userId"))
            .and_then(serde_json::Value::as_str),
        Some("0123456789abcdef0123456789abcdef"),
        "the bucketing digest is what identifies the caller"
    );
}

#[test]
fn the_payload_defaults_are_empty_before_any_request() {
    let payload = EvalPayload::new(attributes());
    assert!(payload.forced_variations.is_empty());
    assert!(payload.forced_features.is_empty());
    assert!(payload.url.is_empty());
}

#[test]
fn every_failure_branch_answers_nothing_and_still_issues_its_request() {
    let branches = [
        (Outcome::Failure, 1_usize),
        (
            Outcome::Answer {
                status: 400,
                body: r#"{"features": {}}"#.to_owned(),
            },
            1,
        ),
        (
            Outcome::Answer {
                status: 500,
                body: r#"{"features": {}}"#.to_owned(),
            },
            1,
        ),
        (Outcome::ok("<html>nope</html>"), 1),
        (Outcome::ok(r#"{"features": 17}"#), 1),
    ];
    for (outcome, expected_requests) in branches {
        let (client, transport) = client_over(outcome);
        let answered = runtime().block_on(client.evaluate(&attributes()));
        assert!(answered.is_none(), "a failure resolves to no state");
        assert_eq!(transport.request_count(), expected_requests);
    }
}

#[test]
fn every_failure_reports_once_at_warning_level_and_names_its_cause() {
    let failures = [
        EvalFailure::Transport,
        EvalFailure::Status(404),
        EvalFailure::Body,
        EvalFailure::Payload,
    ];
    let mut messages = Vec::new();
    for failure in &failures {
        let (level, message) = failure.report();
        assert_eq!(level, LogLevel::Warning, "{failure:?} warns");
        assert!(!message.is_empty(), "{failure:?} says something");
        messages.push(message);
    }
    assert!(
        messages[1].contains("404"),
        "the status branch names the status: {}",
        messages[1]
    );
    messages.sort();
    messages.dedup();
    assert_eq!(
        messages.len(),
        failures.len(),
        "each cause reads differently"
    );
}

#[test]
fn a_status_below_four_hundred_is_read_rather_than_refused() {
    let (client, _transport) = client_over(Outcome::Answer {
        status: 399,
        body: r#"{"features": {"vibe_cli_system_prompt": {"defaultValue": "cli"}}}"#.to_owned(),
    });
    let answered = runtime().block_on(client.evaluate(&attributes()));
    assert_eq!(
        answered.map(|response| response.features.len()),
        Some(1),
        "the reference stops at 400, not before"
    );
}

#[test]
fn closing_is_idempotent_and_closing_an_unused_client_does_nothing() {
    let (client, transport) = client_over(Outcome::ok(r#"{"features": {}}"#));
    let runtime = runtime();
    runtime.block_on(async {
        client.close().await;
        client.close().await;
    });
    assert_eq!(transport.closes(), 1, "the transport is released once");

    // A client that never issued a request never built a transport.
    let unused = RemoteEvalClient::with_url(Some(ORACLE_URL.to_owned()));
    runtime.block_on(async {
        unused.close().await;
        unused.close().await;
    });
}

/// One HTTP request read off a socket: the headers, then as much of the body as
/// `content-length` announces.
fn read_request(stream: &mut TcpStream) -> String {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 1024];
    while let Ok(read) = stream.read(&mut chunk) {
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
        let text = String::from_utf8_lossy(&buffer).into_owned();
        let Some((headers, body)) = text.split_once("\r\n\r\n") else {
            continue;
        };
        let announced = headers
            .to_lowercase()
            .lines()
            .find_map(|line| line.strip_prefix("content-length:")?.trim().parse().ok())
            .unwrap_or(0_usize);
        if body.len() >= announced {
            return text;
        }
    }
    String::from_utf8_lossy(&buffer).into_owned()
}

/// The production transport, driven end to end over a loopback listener: the
/// lazy branch builds its own `reqwest` client, posts the payload and reads the
/// answer back. Every other test here stands a recorder where the connection
/// would be, so this is the one that proves the branch a running binary takes.
#[test]
fn the_production_transport_posts_over_the_wire_and_reads_the_answer_back() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("fixture listener");
    let address = listener.local_addr().expect("listener address").to_string();
    let served = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("the fixture accepts");
        let request = read_request(&mut stream);
        let body = r#"{"features": {"vibe_cli_system_prompt": {"defaultValue": "cli"}}}"#;
        let _ = stream.write_all(
            format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        );
        let _ = stream.flush();
        request
    });

    let client = RemoteEvalClient::with_url(Some(format!("http://{address}/api/eval/sdk-key")));
    let answered = runtime().block_on(async {
        let answered = client.evaluate(&attributes()).await;
        client.close().await;
        answered
    });
    assert_eq!(
        answered.map(|response| response.features.len()),
        Some(1),
        "what the socket carried is what the client answers"
    );

    let request = served.join().expect("the fixture thread finishes");
    assert!(
        request.starts_with("POST /api/eval/sdk-key "),
        "the verb and the path reach the endpoint: {request}"
    );
    assert!(
        request.contains(r#""forcedVariations":{},"forcedFeatures":[],"url":"""#),
        "the three empty defaults reach the wire: {request}"
    );
}

#[test]
fn the_two_spellings_of_the_timeout_agree() {
    assert!((EVAL_REQUEST_TIMEOUT_SECONDS - 5.0).abs() < f64::EPSILON);
    assert!(
        (EVAL_REQUEST_TIMEOUT.as_secs_f64() - EVAL_REQUEST_TIMEOUT_SECONDS).abs() < f64::EPSILON,
        "the duration the client is built with is the constant the corpus records"
    );
}
