//! The stdio transport: one editor connection, read as newline-delimited
//! JSON-RPC frames and answered on a single writer.

pub(crate) mod client;
pub(crate) mod driver;

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinSet;
use vibe_acp::{AcpAgent, AcpAuthEnvironment, AcpExperiments};
use vibe_app_server::client::TurnDriver;
use vibe_app_server::harness::HarnessSelection;
use vibe_app_server::transport::read_bounded_frame;
use vibe_core::telemetry::{ReqwestTelemetryTransport, TelemetryEventObserver};

use crate::stdio::client::{StdioClientPort, WriterMessage, writer_loop};

/// What one editor session is opened with, beyond its transport and its driver.
pub(crate) struct StdioOptions {
    pub(crate) session_root: Option<PathBuf>,
    pub(crate) credential_environment: String,
    pub(crate) auth_environment: Arc<dyn AcpAuthEnvironment>,
    pub(crate) production_cloud: bool,
    pub(crate) telemetry: Option<Arc<TelemetryEventObserver<ReqwestTelemetryTransport>>>,
    pub(crate) experiments: Option<AcpExperiments>,
    pub(crate) harness: HarnessSelection,
}

/// Serves one editor connection until its input ends.
///
/// The reference's connection reads one JSON message per line: a line that is
/// not JSON is skipped, a message without a method answers one of the agent's
/// own requests, a message carrying an `id` member is a request, and any other
/// message is a notification. Each request runs on its own task, so a prompt
/// never holds up the cancellation sent after it.
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
    let (writer_tx, writer_rx) = mpsc::unbounded_channel();
    let writer_task = tokio::spawn(writer_loop(writer, writer_rx));
    let client = Arc::new(StdioClientPort::new(writer_tx.clone()));
    let telemetry = options.telemetry.clone();
    let agent = Arc::new(build_agent(driver, options, &client)?);
    let mut tasks = JoinSet::new();

    while let Some(frame) = read_bounded_frame(&mut reader).await? {
        while tasks.try_join_next().is_some() {}
        let Ok(Value::Object(message)) = serde_json::from_slice::<Value>(&frame) else {
            continue;
        };
        let Some(method) = message.get("method").cloned() else {
            client.resolve(&Value::Object(message));
            continue;
        };
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        let agent = Arc::clone(&agent);
        if let Some(id) = message.get("id").cloned() {
            let client = Arc::clone(&client);
            tasks.spawn(async move {
                let (outcome, followups) =
                    agent.handle_request_with_followups(&method, params).await;
                let response = match outcome {
                    Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
                    Err(error) => {
                        json!({"jsonrpc": "2.0", "id": id, "error": error.json_rpc_error()})
                    }
                };
                client.send(response);
                followups.deliver(client.as_ref());
            });
        } else {
            tasks.spawn(async move {
                agent.handle_notification(&method, params).await;
            });
        }
    }

    client.disconnect();
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
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
        .map_err(|_| "the ACP writer stopped before shutdown")?;
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
        harness,
    } = options;
    let mut agent = AcpAgent::new(driver)?.with_harness_selection(harness);
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
