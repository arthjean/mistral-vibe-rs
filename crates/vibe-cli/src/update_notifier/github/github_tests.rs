use super::*;

/// A one-shot HTTP server on loopback. Update discovery must never leave
/// the machine during tests, and the returned string is the request the
/// gateway actually sent.
fn serve(response: String) -> (String, std::thread::JoinHandle<String>) {
    use std::io::Write as _;

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("fixture listener");
    let address = listener.local_addr().expect("listener address").to_string();
    let handle = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("the fixture accepts");
        let request = read_request(&mut stream);
        let _ = stream.write_all(response.as_bytes());
        let _ = stream.flush();
        request
    });
    (format!("http://{address}"), handle)
}

fn read_request(stream: &mut std::net::TcpStream) -> String {
    use std::io::Read as _;

    let mut request = Vec::new();
    let mut byte = [0_u8; 1];
    while !request.ends_with(b"\r\n\r\n") {
        match stream.read(&mut byte) {
            Ok(0) | Err(_) => break,
            Ok(_) => request.push(byte[0]),
        }
    }
    String::from_utf8_lossy(&request).to_lowercase()
}

fn http_response(status: &str, headers: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
         {headers}\r\n{body}",
        body.len()
    )
}

fn github_gateway(base_url: &str) -> GitHubUpdateGateway {
    GitHubUpdateGateway::with_base_url("arthjean", "mistral-vibe-rs", base_url).expect("gateway")
}

#[test]
fn github_selection_takes_the_newest_published_release() {
    let payload = serde_json::json!([
        {"tag_name": "V2.24.0", "published_at": "2026-02-01T00:00:00Z"},
        {"tag_name": "v2.27.0", "published_at": "2026-05-01T00:00:00Z", "prerelease": true},
        {"tag_name": "v2.26.0", "published_at": "2026-04-01T00:00:00Z", "draft": true},
        {"tag_name": "v2.25.0", "published_at": "2026-03-01T00:00:00Z"},
    ]);
    assert_eq!(
        select_latest_release(&payload).as_deref(),
        Some("2.25.0"),
        "a draft and a prerelease are skipped even when published later"
    );
    // The newest release carrying no usable tag falls through to the next.
    let tagless = serde_json::json!([
        {"tag_name": "  ", "published_at": "2026-05-01T00:00:00Z"},
        {"tag_name": "v", "published_at": "2026-04-01T00:00:00Z"},
        {"tag_name": "2.24.0", "published_at": "2026-02-01T00:00:00Z"},
    ]);
    assert_eq!(select_latest_release(&tagless).as_deref(), Some("2.24.0"));
    // A release with no publication date sorts last, not first.
    let undated = serde_json::json!([
        {"tag_name": "v9.9.9"},
        {"tag_name": "v2.24.0", "published_at": "2026-02-01T00:00:00Z"},
    ]);
    assert_eq!(select_latest_release(&undated).as_deref(), Some("2.24.0"));
    // An empty list is no update rather than an error.
    assert_eq!(select_latest_release(&serde_json::json!([])), None);
    assert_eq!(
        select_latest_release(&serde_json::json!([
            {"tag_name": "v3.0.0", "draft": true}
        ])),
        None
    );
    assert_eq!(select_latest_release(&serde_json::json!({})), None);

    assert_eq!(
        extract_release_version(" v2.24.0 ").as_deref(),
        Some("2.24.0")
    );
    assert_eq!(
        extract_release_version("V2.24.0").as_deref(),
        Some("2.24.0")
    );
    assert_eq!(extract_release_version("2.24.0").as_deref(), Some("2.24.0"));
    assert_eq!(extract_release_version("v"), None);
    assert_eq!(extract_release_version("   "), None);
}

#[test]
fn github_causes_follow_the_reference_branch_order() {
    assert_eq!(
        github_status_cause(429, None),
        Some(UpdateGatewayCause::TooManyRequests)
    );
    assert_eq!(
        github_status_cause(200, Some("0")),
        Some(UpdateGatewayCause::TooManyRequests),
        "the reference reads the budget header before the status, on any status"
    );
    assert_eq!(
        github_status_cause(403, Some("0")),
        Some(UpdateGatewayCause::TooManyRequests),
        "an exhausted budget reports the limit rather than the 403 it arrives as"
    );
    assert_eq!(
        github_status_cause(403, Some("57")),
        Some(UpdateGatewayCause::Forbidden)
    );
    assert_eq!(
        github_status_cause(404, None),
        Some(UpdateGatewayCause::NotFound)
    );
    assert_eq!(
        github_status_cause(500, None),
        Some(UpdateGatewayCause::ErrorResponse)
    );
    assert_eq!(
        github_status_cause(422, Some("57")),
        Some(UpdateGatewayCause::ErrorResponse)
    );
    assert_eq!(github_status_cause(200, Some("57")), None);
    assert_eq!(github_status_cause(304, None), None);
}

#[test]
fn the_not_found_sentence_is_this_port_s_own() {
    // `NOTICE`: the reference sentence is never committed as text. It is
    // recorded as a length plus a SHA-256 measured from
    // `vibe/cli/update_notifier/adapters/github_update_gateway.py` at
    // `vibe_core::parity::REFERENCE_COMMIT`, so this port's own sentence can be
    // held permanently unequal to it.
    const REFERENCE_LENGTH: usize = 88;
    const REFERENCE_DIGEST: &str =
        "db0a0fb435e9d4b4c3100840ef7749c9810fc581102d3e67de0f9ab084e9f630";

    let sentence = GitHubUpdateGateway::NOT_FOUND_MESSAGE;
    assert!(
        !sentence.is_empty(),
        "the not-found cause must still name a next action"
    );
    let digest = vibe_core::text::sha256_hex(sentence.as_bytes());
    assert!(
        sentence.len() != REFERENCE_LENGTH || digest != REFERENCE_DIGEST,
        "the not-found sentence became the reference's own; write an original one"
    );
    assert_ne!(digest, REFERENCE_DIGEST);
}

#[tokio::test]
async fn the_github_gateway_asks_for_the_repository_releases_and_selects_one() {
    let body = serde_json::json!([
        {"tag_name": "v2.24.0", "published_at": "2026-02-01T00:00:00Z"},
        {"tag_name": "v2.25.0", "published_at": "2026-03-01T00:00:00Z"},
    ])
    .to_string();
    let (base_url, served) = serve(http_response("200 OK", "", &body));
    let gateway = github_gateway(&base_url).with_token(Some("token-value".to_owned()));
    assert_eq!(
        gateway.releases_url(),
        format!("{base_url}/repos/arthjean/mistral-vibe-rs/releases")
    );
    assert_eq!(
        gateway.fetch_update().await.expect("fetch"),
        Some("2.25.0".to_owned())
    );
    let request = served.join().expect("fixture thread");
    assert!(
        request.starts_with("get /repos/arthjean/mistral-vibe-rs/releases "),
        "the gateway asked for {request}"
    );
    assert!(request.contains("accept: application/vnd.github+json"));
    assert!(request.contains("user-agent: mistral-vibe-rs-update-notifier/"));
    assert!(request.contains("authorization: bearer token-value"));

    // An empty token is no token, which is what keeps an unauthenticated
    // check anonymous rather than sending `Bearer `.
    let (base_url, served) = serve(http_response("200 OK", "", "[]"));
    let anonymous = github_gateway(&base_url).with_token(Some(String::new()));
    assert_eq!(anonymous.fetch_update().await.expect("fetch"), None);
    assert!(
        !served
            .join()
            .expect("fixture thread")
            .contains("authorization:")
    );
}

#[tokio::test]
async fn the_github_gateway_maps_every_failure_the_reference_maps() {
    let cases = [
        ("404 Not Found", "", "{}", UpdateGatewayCause::NotFound),
        ("403 Forbidden", "", "{}", UpdateGatewayCause::Forbidden),
        (
            "429 Too Many Requests",
            "",
            "{}",
            UpdateGatewayCause::TooManyRequests,
        ),
        (
            "200 OK",
            "x-ratelimit-remaining: 0\r\n",
            "[]",
            UpdateGatewayCause::TooManyRequests,
        ),
        (
            "500 Internal Server Error",
            "",
            "{}",
            UpdateGatewayCause::ErrorResponse,
        ),
        (
            "200 OK",
            "",
            "not json at all",
            UpdateGatewayCause::InvalidResponse,
        ),
    ];
    for (status, headers, body, expected) in cases {
        let (base_url, served) = serve(http_response(status, headers, body));
        let error = github_gateway(&base_url)
            .fetch_update()
            .await
            .expect_err("the gateway reports a cause");
        assert_eq!(error.cause, expected, "status {status} mapped wrongly");
        if expected == UpdateGatewayCause::NotFound {
            assert_eq!(error.user_message(), GitHubUpdateGateway::NOT_FOUND_MESSAGE);
        } else {
            assert_eq!(error.user_message(), expected.default_message());
        }
        let _ = served.join();
    }

    // A refused connection is a transport failure, not a status.
    let refused =
        GitHubUpdateGateway::with_base_url("arthjean", "mistral-vibe-rs", "http://127.0.0.1:9")
            .expect("gateway");
    assert_eq!(
        refused.fetch_update().await.expect_err("no listener").cause,
        UpdateGatewayCause::RequestFailed
    );
}

#[tokio::test]
async fn a_silent_server_fails_the_check_within_the_gateway_timeout() {
    assert_eq!(
        GATEWAY_TIMEOUT,
        Duration::from_secs(5),
        "the reference gives the gateway five seconds"
    );
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("fixture listener");
    let address = listener.local_addr().expect("listener address").to_string();
    let silent = std::thread::spawn(move || {
        let accepted = listener.accept();
        std::thread::sleep(Duration::from_millis(500));
        drop(accepted);
    });
    let gateway = GitHubUpdateGateway::with_timeout(
        "arthjean",
        "mistral-vibe-rs",
        format!("http://{address}"),
        Duration::from_millis(100),
    )
    .expect("gateway");
    let started = std::time::Instant::now();
    let error = gateway
        .fetch_update()
        .await
        .expect_err("a server that never answers");
    assert_eq!(error.cause, UpdateGatewayCause::RequestFailed);
    assert!(
        started.elapsed() < Duration::from_millis(400),
        "the check returned the timeout rather than waiting for the server"
    );
    let _ = silent.join();
}
