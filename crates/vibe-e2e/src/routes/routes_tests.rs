use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::platform::Platform;
use crate::routes::RouteReply;

fn call(platform: &Platform, method: &str, path: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(platform.address()).unwrap();
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nhost: localhost\r\ncontent-length: 2\r\n\r\n{{}}"
    )
    .unwrap();
    let mut raw = String::new();
    stream.read_to_string(&mut raw).unwrap();
    let (head, body) = raw.split_once("\r\n\r\n").unwrap();
    let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    (status, body.to_owned())
}

#[test]
fn every_service_route_answers_its_neutral_default() {
    let platform = Platform::start().unwrap();
    let cases: [(&str, &str, u16, Value); 8] = [
        (
            "GET",
            "/api/v1/code/managed-config",
            200,
            json!({"state": "disabled"}),
        ),
        (
            "GET",
            "/api/vibe/whoami",
            404,
            json!({"detail": "Not Found"}),
        ),
        ("GET", "/v1/users/me", 404, json!({"detail": "Not Found"})),
        ("POST", "/v1/datalake/events", 200, json!({})),
        ("POST", "/api/eval/sdk-e2e", 200, json!({"features": {}})),
        (
            "GET",
            "/v1/connectors/bootstrap?include_auth_actionable_connectors=true",
            200,
            json!({"connectors": []}),
        ),
        (
            "POST",
            "/v1/connectors-gateway/abc/mcp",
            404,
            json!({"detail": "Not Found"}),
        ),
        (
            "GET",
            "/pypi/mistral-vibe/json",
            200,
            json!({"info": {"version": env!("CARGO_PKG_VERSION")}, "releases": {}}),
        ),
    ];
    for (method, path, status, body) in cases {
        let (got, text) = call(&platform, method, path);
        assert_eq!(got, status, "{method} {path}");
        assert_eq!(
            serde_json::from_str::<Value>(&text).unwrap(),
            body,
            "{method} {path}"
        );
    }
    let (status, text) = call(&platform, "GET", "/repos/arthjean/mistral-vibe-rs/releases");
    assert_eq!(status, 200);
    assert!(
        text.contains(&format!("v{}", env!("CARGO_PKG_VERSION"))),
        "{text}"
    );
    let (status, text) = call(&platform, "GET", "/pages/anything");
    assert_eq!(status, 200);
    assert!(text.contains("e2e page"), "{text}");
    assert!(platform.failures().is_empty(), "{:?}", platform.failures());
}

#[test]
fn every_route_lands_in_the_journal_under_its_name() {
    let platform = Platform::start().unwrap();
    call(&platform, "POST", "/v1/datalake/events");
    call(&platform, "GET", "/api/vibe/whoami");
    let queues: Vec<String> = platform.journal().into_iter().map(|e| e.queue).collect();
    assert_eq!(queues, ["datalake", "whoami"]);
}

#[test]
fn an_override_replaces_the_status_the_body_and_the_delay() {
    let platform = Platform::start().unwrap();
    platform.route(
        "managed_config",
        RouteReply::json(503, &json!({"error": "down"})).delay(150),
    );
    platform.page("docs", "<p>scenario page</p>");
    let started = Instant::now();
    let (status, body) = call(&platform, "GET", "/api/v1/code/managed-config");
    assert!(started.elapsed() >= Duration::from_millis(150));
    assert_eq!(status, 503);
    assert_eq!(body, r#"{"error":"down"}"#);
    let (_, page) = call(&platform, "GET", "/pages/docs");
    assert_eq!(page, "<p>scenario page</p>");
    assert!(platform.page_url("docs").ends_with("/pages/docs"));
}

#[test]
fn an_unknown_path_answers_404_and_fails_the_trial() {
    let platform = Platform::start().unwrap();
    let (status, _) = call(&platform, "DELETE", "/v2/nowhere");
    assert_eq!(status, 404);
    assert_eq!(
        platform.failures(),
        ["unexpected request DELETE /v2/nowhere: no platform route serves it"]
    );
    assert_eq!(platform.journal()[0].queue, "unknown");
}
