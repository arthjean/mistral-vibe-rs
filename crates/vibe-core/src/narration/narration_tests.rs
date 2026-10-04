use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::*;
use crate::llm::MapCredentials;
use crate::provider::config::ApiSettings;

/// Answers one request with `status` and `body`, and hands back the request
/// it read: its head and its JSON body.
async fn one_shot(status: u16, body: Value) -> (String, tokio::task::JoinHandle<(String, Value)>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("a port");
    let base = format!("http://{}/v1", listener.local_addr().expect("an address"));
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("a connection");
        let mut buffer = Vec::new();
        let mut piece = [0_u8; 8_192];
        let head_end = loop {
            if let Some(position) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
                break position;
            }
            let read = socket.read(&mut piece).await.expect("a read");
            buffer.extend_from_slice(&piece[..read]);
        };
        let head = String::from_utf8_lossy(&buffer[..head_end]).to_ascii_lowercase();
        let length: usize = head
            .lines()
            .find_map(|line| line.strip_prefix("content-length:"))
            .and_then(|value| value.trim().parse().ok())
            .unwrap_or(0);
        while buffer.len() < head_end + 4 + length {
            let read = socket.read(&mut piece).await.expect("a read");
            buffer.extend_from_slice(&piece[..read]);
        }
        let request = serde_json::from_slice(&buffer[head_end + 4..]).unwrap_or(Value::Null);
        let payload = body.to_string();
        let response = format!(
            "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
             connection: close\r\n\r\n{payload}",
            payload.len()
        );
        socket
            .write_all(response.as_bytes())
            .await
            .expect("a write");
        (head, request)
    });
    (base, task)
}

fn provider(name: &str, base: &str, key_variable: &str) -> ProviderConfig {
    let mut provider = ProviderConfig::new(name, base);
    provider.backend = BackendKind::Mistral;
    provider.api_key_env_var = key_variable.to_owned();
    provider
}

fn context(keys: &[(&str, &str)]) -> BackendContext {
    let keys = keys
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect::<BTreeMap<_, _>>();
    BackendContext::ambient(ApiSettings::default(), Arc::new(MapCredentials(keys)))
}

fn completion(content: Value) -> Value {
    json!({
        "id": "summary-1",
        "object": "chat.completion",
        "model": "mistral-vibe-cli-fast",
        "created": 0,
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": content},
            "finish_reason": "stop",
        }],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
    })
}

const INPUT: TurnSummaryInput<'static> = TurnSummaryInput {
    user_message: "fix the parser",
    assistant_text: "Fixed it.",
    error: Some("Rate limits exceeded."),
};

#[test]
fn the_user_message_titles_each_part_the_turn_produced() {
    assert_eq!(
        user_content(&INPUT),
        "## User Request\nfix the parser\n\n## Assistant Response\nFixed it.\n\n\
         ## Error\nRate limits exceeded."
    );
    assert_eq!(
        user_content(&TurnSummaryInput {
            user_message: "",
            assistant_text: "",
            error: None,
        }),
        "## User Request\n"
    );
}

#[tokio::test]
async fn the_fast_model_summarizes_on_the_mistral_provider() {
    let (base, request) = one_shot(200, completion(json!("Parser fixed."))).await;
    let providers = [
        provider("other", "http://127.0.0.1:9/v1", ""),
        provider(NARRATION_PROVIDER, &base, "NARRATION_KEY"),
    ];
    let metadata = Map::from_iter([("call_type".to_owned(), json!("secondary_call"))]);
    let summary = summarize(
        &providers,
        &context(&[("NARRATION_KEY", "secret")]),
        &INPUT,
        &metadata,
    )
    .await;
    assert_eq!(summary.as_deref(), Some("Parser fixed."));
    let (head, body) = request.await.expect("the request");
    assert!(head.contains("authorization: bearer secret"), "{head}");
    assert!(
        head.contains("user-agent: mistral-client-python/mistral-vibe/"),
        "{head}"
    );
    assert_eq!(body["model"], "mistral-vibe-cli-fast");
    assert_eq!(body["temperature"], 0.0);
    assert_eq!(body["max_tokens"], 512);
    assert_eq!(body["metadata"]["call_type"], "secondary_call");
    assert_eq!(body["messages"][0]["role"], "system");
    assert_eq!(body["messages"][1]["content"], user_content(&INPUT));
}

/// Reference `result.message.content or ""`.
#[tokio::test]
async fn an_answer_without_content_is_an_empty_summary() {
    let (base, request) = one_shot(200, completion(Value::Null)).await;
    let providers = [provider(NARRATION_PROVIDER, &base, "")];
    let summary = summarize(&providers, &context(&[]), &INPUT, &Map::new()).await;
    assert_eq!(summary.as_deref(), Some(""));
    request.await.expect("the request");
}

#[tokio::test]
async fn no_provider_no_key_or_a_failed_call_is_no_summary() {
    let unnamed = [provider("elsewhere", "http://127.0.0.1:9/v1", "")];
    assert_eq!(
        summarize(&unnamed, &context(&[]), &INPUT, &Map::new()).await,
        None
    );

    let keyless = [provider(
        NARRATION_PROVIDER,
        "http://127.0.0.1:9/v1",
        "UNSET_KEY",
    )];
    assert_eq!(
        summarize(&keyless, &context(&[]), &INPUT, &Map::new()).await,
        None
    );

    let (base, request) = one_shot(400, json!({"message": "bad request"})).await;
    let refused = [provider(NARRATION_PROVIDER, &base, "")];
    assert_eq!(
        summarize(&refused, &context(&[]), &INPUT, &Map::new()).await,
        None
    );
    request.await.expect("the request");
}
