use std::io::{Read as _, Write as _};
use std::net::TcpListener;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use serde_json::{Value, json};

use super::*;

fn spawn_http_response(status: &str, response_body: Value) -> (String, mpsc::Receiver<Vec<u8>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("loopback listener");
    let address = listener.local_addr().expect("loopback address");
    let (sender, receiver) = mpsc::channel();
    let status = status.to_owned();
    let response_body = serde_json::to_vec(&response_body).expect("response JSON");
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("HTTP request");
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("read timeout");
        let mut request = Vec::new();
        let mut buffer = [0_u8; 4 * 1024];
        loop {
            let read = stream.read(&mut buffer).expect("HTTP request bytes");
            if read == 0 {
                break;
            }
            request.extend_from_slice(&buffer[..read]);
            let Some(headers_end) = request.windows(4).position(|window| window == b"\r\n\r\n")
            else {
                continue;
            };
            let headers_end = headers_end + 4;
            let headers = String::from_utf8_lossy(&request[..headers_end]);
            let content_length = headers.lines().find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            });
            if request.len() >= headers_end.saturating_add(content_length.unwrap_or(0)) {
                break;
            }
        }
        sender.send(request).expect("captured request");
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            response_body.len()
        );
        stream
            .write_all(response.as_bytes())
            .and_then(|()| stream.write_all(&response_body))
            .expect("HTTP response");
    });
    (format!("http://{address}"), receiver)
}

#[tokio::test]
async fn http_auth_expiry_is_typed_and_does_not_retry() {
    let (base_url, captured) = spawn_http_response("401 Unauthorized", json!({}));
    let cloud = VibeCodeHttpCloud::new(
        VibeCodeCloudConfig::new(
            &base_url,
            SecretString::from("expired-credential".to_owned()),
        )
        .expect("cloud config"),
    )
    .expect("HTTP cloud");

    assert!(matches!(
        cloud.list_projects(None).await,
        Err(CloudError::Unauthorized(message)) if message.contains("authenticate")
    ));
    captured
        .recv_timeout(Duration::from_secs(2))
        .expect("single captured HTTP request");
}

#[tokio::test]
async fn http_listing_rejects_missing_required_response_fields() {
    let (base_url, captured) = spawn_http_response(
        "200 OK",
        json!({
            "items": [{"id": "project-1"}],
            "nextCursor": null,
        }),
    );
    let cloud = VibeCodeHttpCloud::new(
        VibeCodeCloudConfig::new(&base_url, SecretString::from("test-credential".to_owned()))
            .expect("cloud config"),
    )
    .expect("HTTP cloud");

    assert!(matches!(
        cloud.list_projects(None).await,
        Err(CloudError::Unavailable(message)) if message.contains("invalid response")
    ));
    captured
        .recv_timeout(Duration::from_secs(2))
        .expect("captured HTTP request");
}
