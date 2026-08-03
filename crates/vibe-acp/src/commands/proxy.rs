//! `/proxy-setup`: read and write proxy and TLS settings in user configuration.

use serde_json::json;
use vibe_app_server::client::TurnDriver;

use crate::commands::CommandSink;

const KEYS: &[&str] = &["HTTP_PROXY", "HTTPS_PROXY", "NO_PROXY", "SSL_CERT_FILE"];
const USAGE: &str = "Usage: `/proxy-setup <HTTP_PROXY|HTTPS_PROXY|NO_PROXY|SSL_CERT_FILE> [value]`";

pub(crate) async fn execute<D>(sink: &CommandSink<'_, D>, arguments: &str) -> String
where
    D: TurnDriver,
{
    let parts = arguments.split_whitespace().collect::<Vec<_>>();
    let (key, value) = match parts.as_slice() {
        [] => return USAGE.to_owned(),
        [key] => ((*key).to_ascii_uppercase(), None),
        [key, value] => ((*key).to_ascii_uppercase(), Some(*value)),
        _ => return "Proxy values containing whitespace are not supported.".to_owned(),
    };
    if !KEYS.contains(&key.as_str()) {
        return "Proxy key must be HTTP_PROXY, HTTPS_PROXY, NO_PROXY, or SSL_CERT_FILE.".to_owned();
    }
    let path = [key.to_ascii_lowercase()];
    let mutation = match value {
        Some(value) => json!({"path": path, "value": value}),
        None => json!({"path": path, "remove": true}),
    };
    let outcome = sink.harness.service.lock().await.public_call(
        "config/batchWrite",
        json!({
            "writes": [{
                "target": "user",
                "expectedFingerprint": null,
                "mutations": [mutation],
            }],
        }),
    );
    match (outcome, value) {
        (Ok(_), Some(_)) => {
            format!("Set `{key}` in user configuration. Start a new session for it to take effect.")
        }
        (Ok(_), None) => format!(
            "Removed `{key}` from user configuration. Start a new session for it to take effect."
        ),
        (Err(error), _) => format!("Proxy configuration failed: {error}"),
    }
}
