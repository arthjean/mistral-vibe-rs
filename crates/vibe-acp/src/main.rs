#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use std::path::PathBuf;
use std::process::ExitCode;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncWrite, AsyncWriteExt, BufReader};
use vibe_acp::{AcpAgent, AcpNewSession};
use vibe_app_server::client::{LiveDriverConfig, LiveTurnDriver};
use vibe_app_server::transport::read_bounded_frame;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireRequest {
    jsonrpc: String,
    id: Value,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Debug, Serialize)]
struct WireResponse {
    jsonrpc: &'static str,
    id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<WireError>,
}

#[derive(Debug, Serialize)]
struct WireError {
    code: i64,
    message: String,
}

#[derive(Debug, Serialize)]
struct WireNotification<T> {
    jsonrpc: &'static str,
    method: &'static str,
    params: T,
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let driver = LiveTurnDriver::from_environment(LiveDriverConfig {
        style: std::env::var("VIBE_PROVIDER_STYLE").unwrap_or_else(|_| "mistral".to_owned()),
        endpoint: std::env::var("VIBE_API_BASE")
            .unwrap_or_else(|_| "https://api.mistral.ai/v1/chat/completions".to_owned()),
        model: std::env::var("VIBE_MODEL").unwrap_or_else(|_| "mistral-medium-3.5".to_owned()),
        credential_environment: std::env::var("VIBE_CREDENTIAL_ENV")
            .unwrap_or_else(|_| "MISTRAL_API_KEY".to_owned()),
        system_prompt: "You are Mistral Vibe.".to_owned(),
        session_root: std::env::var_os("VIBE_HOME")
            .map(PathBuf::from)
            .map(|path| path.join("sessions")),
        input_price_per_million_micros: price_from_environment("VIBE_INPUT_PRICE", 1_500_000)?,
        output_price_per_million_micros: price_from_environment("VIBE_OUTPUT_PRICE", 7_500_000)?,
    })?;
    let mut agent = AcpAgent::new(driver)?;
    let mut stdin = BufReader::new(tokio::io::stdin());
    let mut stdout = tokio::io::stdout();
    let serving = async {
        while let Some(frame) = read_bounded_frame(&mut stdin).await? {
            let response = match serde_json::from_slice::<WireRequest>(&frame) {
                Ok(request) => handle(&mut agent, request, &mut stdout).await,
                Err(error) => WireResponse {
                    jsonrpc: "2.0",
                    id: Value::Null,
                    result: None,
                    error: Some(WireError {
                        code: -32700,
                        message: format!("invalid ACP request: {error}"),
                    }),
                },
            };
            let mut bytes = serde_json::to_vec(&response)?;
            bytes.push(b'\n');
            stdout.write_all(&bytes).await?;
            stdout.flush().await?;
        }
        Ok::<(), Box<dyn std::error::Error>>(())
    }
    .await;
    let disconnect = agent.disconnect();
    serving?;
    disconnect?;
    Ok(())
}

fn price_from_environment(
    name: &str,
    default_micros: u64,
) -> Result<u64, Box<dyn std::error::Error>> {
    let Ok(value) = std::env::var(name) else {
        return Ok(default_micros);
    };
    let price = value.parse::<f64>()?;
    if !price.is_finite() || price < 0.0 || price > u64::MAX as f64 / 1_000_000.0 {
        return Err(format!("{name} must be a finite non-negative number").into());
    }
    Ok((price * 1_000_000.0).round() as u64)
}

async fn handle<W>(
    agent: &mut AcpAgent<LiveTurnDriver>,
    request: WireRequest,
    writer: &mut W,
) -> WireResponse
where
    W: AsyncWrite + Unpin,
{
    let valid_id = request.id.is_i64() || request.id.is_u64() || request.id.is_string();
    let result = if request.jsonrpc != "2.0" || !valid_id {
        Err(vibe_acp::AcpError::Client(
            vibe_app_server::client::ClientError::InvalidResponse(
                "ACP requests require jsonrpc 2.0 and a string or integer ID".to_owned(),
            ),
        ))
    } else {
        match request.method.as_str() {
            "initialize" => agent
                .initialize()
                .and_then(|value| serde_json::to_value(value).map_err(Into::into)),
            "session/new" => serde_json::from_value::<AcpNewSession>(request.params)
                .map_err(|error| {
                    vibe_acp::AcpError::Client(
                        vibe_app_server::client::ClientError::InvalidResponse(error.to_string()),
                    )
                })
                .and_then(|params| agent.new_session(params))
                .and_then(|value| serde_json::to_value(value).map_err(Into::into)),
            "session/prompt" => {
                let session_id = request
                    .params
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                let prompt = prompt_text(request.params.get("prompt"));
                match (session_id, prompt) {
                    (Some(session_id), Some(prompt)) => {
                        let (sender, mut updates) = tokio::sync::mpsc::unbounded_channel();
                        let prompt_future = agent.prompt_streaming(&session_id, &prompt, sender);
                        tokio::pin!(prompt_future);
                        loop {
                            tokio::select! {
                                response = &mut prompt_future => {
                                    break response.and_then(|value| {
                                        serde_json::to_value(value).map_err(Into::into)
                                    });
                                }
                                update = updates.recv() => {
                                    let Some(update) = update else {
                                        continue;
                                    };
                                    if let Err(error) = write_notification(writer, &update).await {
                                        break Err(vibe_acp::AcpError::Client(
                                            vibe_app_server::client::ClientError::InvalidResponse(
                                                error.to_string(),
                                            ),
                                        ));
                                    }
                                }
                            }
                        }
                    }
                    _ => Err(vibe_acp::AcpError::Client(
                        vibe_app_server::client::ClientError::InvalidResponse(
                            "session/prompt requires sessionId and prompt".to_owned(),
                        ),
                    )),
                }
            }
            "session/close" => request
                .params
                .get("sessionId")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    vibe_acp::AcpError::Client(
                        vibe_app_server::client::ClientError::InvalidResponse(
                            "session/close requires sessionId".to_owned(),
                        ),
                    )
                })
                .and_then(|session_id| agent.close_session(session_id))
                .map(|()| json!({})),
            "shutdown" => agent.disconnect().map(|()| json!({})),
            _ => Err(vibe_acp::AcpError::Client(
                vibe_app_server::client::ClientError::InvalidResponse(format!(
                    "unknown ACP method `{}`",
                    request.method
                )),
            )),
        }
    };
    match result {
        Ok(result) => WireResponse {
            jsonrpc: "2.0",
            id: request.id,
            result: Some(result),
            error: None,
        },
        Err(error) => WireResponse {
            jsonrpc: "2.0",
            id: request.id,
            result: None,
            error: Some(WireError {
                code: -32602,
                message: error.to_string(),
            }),
        },
    }
}

fn prompt_text(value: Option<&Value>) -> Option<String> {
    match value {
        Some(Value::String(text)) => Some(text.clone()),
        Some(Value::Array(blocks)) => Some(
            blocks
                .iter()
                .filter_map(|block| {
                    (block.get("type").and_then(Value::as_str) == Some("text"))
                        .then(|| block.get("text").and_then(Value::as_str))
                        .flatten()
                })
                .collect::<Vec<_>>()
                .join("\n\n"),
        ),
        _ => None,
    }
}

async fn write_notification<W>(
    writer: &mut W,
    update: &vibe_acp::AcpSessionUpdate,
) -> Result<(), std::io::Error>
where
    W: AsyncWrite + Unpin,
{
    let mut bytes = serde_json::to_vec(&WireNotification {
        jsonrpc: "2.0",
        method: "session/update",
        params: update,
    })
    .map_err(std::io::Error::other)?;
    bytes.push(b'\n');
    writer.write_all(&bytes).await?;
    writer.flush().await
}
