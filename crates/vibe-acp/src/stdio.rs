//! The stdio transport: one editor connection, read as newline-delimited
//! JSON-RPC frames and answered on a single writer.

pub(crate) mod client;
pub(crate) mod dispatch;
pub(crate) mod driver;
pub(crate) mod wire;

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinSet;
use vibe_acp::{AcpAgent, AcpAuthEnvironment, AcpExperiments};
use vibe_app_server::client::TurnDriver;
use vibe_app_server::transport::read_bounded_frame;
use vibe_core::observability::{LogLevel, log};
use vibe_core::telemetry::{ReqwestTelemetryTransport, TelemetryEventObserver};

use crate::stdio::client::{
    StdioClientPort, WRITER_QUEUE_CAPACITY, WriterMessage, send_value_wait, writer_loop,
};
use crate::stdio::dispatch::handle_request;
use crate::stdio::wire::{WireRequest, acp_error_response, error_response, success_response};

/// The extension notification an editor records telemetry through. The wire
/// name carries the underscore ACP requires on an extension method, which the
/// reference's router strips before dispatching it.
const TELEMETRY_SEND_METHOD: &str = "_telemetry/send";

const MAX_CONCURRENT_REQUESTS: usize = 128;

/// What one editor session is opened with, beyond its transport and its driver.
pub(crate) struct StdioOptions {
    pub(crate) session_root: Option<PathBuf>,
    pub(crate) credential_environment: String,
    pub(crate) auth_environment: Arc<dyn AcpAuthEnvironment>,
    pub(crate) production_cloud: bool,
    pub(crate) telemetry: Option<Arc<TelemetryEventObserver<ReqwestTelemetryTransport>>>,
    pub(crate) experiments: Option<AcpExperiments>,
}

pub(crate) async fn run_stdio<R, W, D>(
    mut reader: R,
    writer: W,
    driver: D,
    options: StdioOptions,
) -> Result<(), Box<dyn std::error::Error>>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
    D: TurnDriver + 'static,
{
    let (writer_tx, writer_rx) = mpsc::channel(WRITER_QUEUE_CAPACITY);
    let writer_task = tokio::spawn(writer_loop(writer, writer_rx));
    let client = Arc::new(StdioClientPort::new(writer_tx.clone()));
    let telemetry = options.telemetry.clone();
    let agent = Arc::new(build_agent(driver, options, &client)?);
    let mut requests = JoinSet::new();

    while let Some(frame) = read_bounded_frame(&mut reader).await? {
        while requests.try_join_next().is_some() {}
        let value = match serde_json::from_slice::<Value>(&frame) {
            Ok(value) => value,
            Err(error) => {
                send_value_wait(
                    &writer_tx,
                    error_response(Value::Null, -32700, format!("invalid ACP JSON: {error}")),
                )
                .await?;
                continue;
            }
        };
        if value.get("method").is_none() {
            if !client.resolve(&value) {
                send_value_wait(
                    &writer_tx,
                    error_response(
                        value.get("id").cloned().unwrap_or(Value::Null),
                        -32600,
                        "unmatched ACP response".to_owned(),
                    ),
                )
                .await?;
            }
            continue;
        }
        let request = match serde_json::from_value::<WireRequest>(value) {
            Ok(request) => request,
            Err(error) => {
                send_value_wait(
                    &writer_tx,
                    error_response(Value::Null, -32600, format!("invalid ACP request: {error}")),
                )
                .await?;
                continue;
            }
        };
        if request.method == "session/cancel" && request.id.is_null() {
            if let Some(session_id) = request.params.get("sessionId").and_then(Value::as_str) {
                let _ = agent.cancel(session_id).await;
            }
            continue;
        }
        // Extension notifications reach the wire under a leading underscore and
        // carry no id. The reference's router hands every one to
        // `ext_notification`, which serves `telemetry/send` and returns on any
        // other name, and its connection swallows what the handler raises
        // because a notification is answered with nothing.
        if request.id.is_null() && request.method.starts_with('_') {
            if request.method == TELEMETRY_SEND_METHOD
                && let Err(error) = agent.telemetry_notification(&request.params).await
            {
                log(
                    LogLevel::Warning,
                    &format!("Dropping an ACP telemetry notification: {error}"),
                );
            }
            continue;
        }
        if request.method == "shutdown" {
            let response = match agent.disconnect().await {
                Ok(()) => success_response(request.id, json!({})),
                Err(error) => acp_error_response(request.id, error),
            };
            send_value_wait(&writer_tx, response).await?;
            break;
        }
        if requests.len() >= MAX_CONCURRENT_REQUESTS {
            send_value_wait(
                &writer_tx,
                error_response(
                    request.id,
                    -32002,
                    format!(
                        "ACP request concurrency exceeds the {MAX_CONCURRENT_REQUESTS}-request limit"
                    ),
                ),
            )
            .await?;
            continue;
        }
        let agent = agent.clone();
        let writer = writer_tx.clone();
        requests.spawn(async move {
            handle_request(agent, request, writer).await;
        });
    }

    client.disconnect();
    requests.abort_all();
    while requests.join_next().await.is_some() {}
    agent.disconnect().await?;
    // Reference `TelemetryClient.aclose`: a delivery already in flight is
    // awaited before the process leaves, so a last event is not lost to the
    // shutdown that raised it.
    if let Some(telemetry) = telemetry {
        telemetry.flush().await;
    }
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    writer_tx
        .send(WriterMessage::Shutdown(shutdown_tx))
        .await
        .map_err(|_| "ACP writer stopped before shutdown")?;
    let _ = shutdown_rx.await;
    writer_task.await??;
    Ok(())
}

fn build_agent<D>(
    driver: D,
    options: StdioOptions,
    client: &Arc<StdioClientPort>,
) -> Result<AcpAgent<D>, vibe_acp::AcpError>
where
    D: TurnDriver + 'static,
{
    let StdioOptions {
        session_root,
        credential_environment,
        auth_environment,
        production_cloud,
        telemetry,
        experiments,
    } = options;
    let mut agent = AcpAgent::new(driver)?;
    if let Some(telemetry) = telemetry {
        agent = agent.with_client_telemetry(telemetry);
    }
    if let Some(experiments) = experiments {
        agent = agent.with_experiments(experiments);
    }
    if let Some(session_root) = session_root {
        agent = agent.with_session_root(session_root);
    }
    if production_cloud {
        agent = agent.with_production_cloud();
    }
    Ok(agent
        .with_credential_environment(credential_environment)
        .with_auth_environment(auth_environment)
        .with_client_port(client.clone(), vibe_acp::DEFAULT_CLIENT_TOOL_TIMEOUT))
}
