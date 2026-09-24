//! The editor answering for the filesystem and the terminal.
//!
//! The app server delegates a tool to the client that declared it by sending a
//! `clientTool/*` request. Reference `AcpClientToolHandler`
//! (`vibe/acp/tool_io.py`) answers each one by making the matching ACP
//! request of the editor, under the session the client knows, and announces a
//! terminal it created on the tool call that asked for it.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Map, Value, json};
use tokio::sync::{mpsc, oneshot};
use vibe_app_server::client_tools::ClientToolBridge;
use vibe_protocol::{ClientToolCapability, RequestId};

use crate::protocol::AcpClientCapabilities;

pub const DEFAULT_CLIENT_TOOL_TIMEOUT: Duration = Duration::from_secs(30);

pub type AcpClientFuture<'a> = Pin<Box<dyn Future<Output = Result<Value, String>> + Send + 'a>>;

pub trait AcpClientPort: Send + Sync {
    fn request<'a>(&'a self, method: &'a str, params: Value) -> AcpClientFuture<'a>;

    /// Sends a notification, queued behind everything already written so it
    /// reaches the client in the order it was produced.
    fn notify(&self, method: &str, params: Value);
}

/// The tools the editor hosts, in the vocabulary the app server gates on.
/// Reference `_client_descriptor`.
pub(crate) fn declared_client_tools(
    capabilities: &AcpClientCapabilities,
) -> Vec<ClientToolCapability> {
    [
        (
            capabilities.fs.read_text_file,
            ClientToolCapability::FilesystemRead,
        ),
        (
            capabilities.fs.write_text_file,
            ClientToolCapability::FilesystemWrite,
        ),
        (capabilities.terminal, ClientToolCapability::Terminal),
    ]
    .into_iter()
    .filter_map(|(declared, capability)| declared.then_some(capability))
    .collect()
}

/// Holds a delegated request back until the running turn has forwarded every
/// update raised before it, so the editor sees the tool call before the tool
/// reaches for its files: the reference writes both on one connection, in the
/// order they happen.
#[derive(Clone, Default)]
pub(crate) struct UpdateBarrier(Arc<Mutex<Option<mpsc::UnboundedSender<oneshot::Sender<()>>>>>);

impl UpdateBarrier {
    /// Hands the running turn the requests to flush, until the guard drops.
    pub(crate) fn install(&self) -> (mpsc::UnboundedReceiver<oneshot::Sender<()>>, BarrierGuard) {
        let (sender, receiver) = mpsc::unbounded_channel();
        if let Ok(mut slot) = self.0.lock() {
            *slot = Some(sender);
        }
        (receiver, BarrierGuard(self.clone()))
    }

    async fn wait(&self) {
        let sender = self.0.lock().ok().and_then(|slot| slot.clone());
        if let Some(sender) = sender {
            let (acknowledge, flushed) = oneshot::channel();
            if sender.send(acknowledge).is_ok() {
                let _ = flushed.await;
            }
        }
    }
}

/// Removes the barrier when the turn that installed it ends.
pub(crate) struct BarrierGuard(UpdateBarrier);

impl Drop for BarrierGuard {
    fn drop(&mut self) {
        if let Ok(mut slot) = (self.0).0.lock() {
            *slot = None;
        }
    }
}

/// Claims the write side of `bridge`, which must happen before the session
/// registers its tools: a bridge with nowhere to send keeps them local.
pub(crate) fn attach_client_tools(bridge: &ClientToolBridge) -> mpsc::UnboundedReceiver<Vec<u8>> {
    let (sender, frames) = mpsc::unbounded_channel::<Vec<u8>>();
    bridge.attach(sender);
    frames
}

/// Answers every `clientTool/*` request `bridge` sends for the session the
/// client knows as `session_id`, until the bridge lets go.
pub(crate) fn serve_client_tools(
    client: Arc<dyn AcpClientPort>,
    bridge: &Arc<ClientToolBridge>,
    mut frames: mpsc::UnboundedReceiver<Vec<u8>>,
    session_id: String,
    barrier: UpdateBarrier,
) -> tokio::task::JoinHandle<()> {
    let bridge = Arc::clone(bridge);
    tokio::spawn(async move {
        while let Some(frame) = frames.recv().await {
            let Ok(request) = serde_json::from_slice::<Value>(&frame) else {
                continue;
            };
            let Some(id) = request
                .get("id")
                .cloned()
                .and_then(|id| serde_json::from_value::<RequestId>(id).ok())
            else {
                continue;
            };
            let client = Arc::clone(&client);
            let bridge = Arc::clone(&bridge);
            let session_id = session_id.clone();
            let barrier = barrier.clone();
            tokio::spawn(async move {
                barrier.wait().await;
                let method = request
                    .get("method")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let params = request.get("params").cloned().unwrap_or(Value::Null);
                let result = answer(client.as_ref(), &session_id, method, &params).await;
                bridge.resolve(&id, result);
            });
        }
    })
}

/// The ACP request one delegated call becomes, and the answer it is read
/// back as.
async fn answer(
    client: &dyn AcpClientPort,
    session_id: &str,
    method: &str,
    params: &Value,
) -> Result<Value, String> {
    let field = |key: &str| params.get(key).cloned().unwrap_or(Value::Null);
    let request = |pairs: &[(&str, Value)]| {
        let mut request = Map::new();
        request.insert("sessionId".to_owned(), json!(session_id));
        for (key, value) in pairs {
            if !value.is_null() {
                request.insert((*key).to_owned(), value.clone());
            }
        }
        Value::Object(request)
    };
    let terminal = || request(&[("terminalId", field("terminalId"))]);
    match method {
        "clientTool/readTextFile" => {
            let response = client
                .request(
                    "fs/read_text_file",
                    request(&[
                        ("path", field("path")),
                        ("line", field("line")),
                        ("limit", field("limit")),
                    ]),
                )
                .await?;
            Ok(json!({"content": response.get("content").cloned().unwrap_or(json!(""))}))
        }
        "clientTool/writeTextFile" => {
            client
                .request(
                    "fs/write_text_file",
                    request(&[("path", field("path")), ("content", field("content"))]),
                )
                .await?;
            Ok(json!({}))
        }
        "clientTool/terminal/create" => {
            let env = params.get("env").and_then(Value::as_object).map(|env| {
                Value::Array(
                    env.iter()
                        .map(|(name, value)| json!({"name": name, "value": value}))
                        .collect(),
                )
            });
            let response = client
                .request(
                    "terminal/create",
                    request(&[
                        ("command", field("command")),
                        ("args", field("args")),
                        ("env", env.unwrap_or(Value::Null)),
                        ("cwd", field("cwd")),
                        ("outputByteLimit", field("outputByteLimit")),
                    ]),
                )
                .await?;
            let terminal_id = response.get("terminalId").cloned().unwrap_or(Value::Null);
            if let Some(tool_call_id) = params.get("toolCallId").filter(|id| !id.is_null()) {
                client.notify(
                    "session/update",
                    json!({
                        "sessionId": session_id,
                        "update": {
                            "sessionUpdate": "tool_call_update",
                            "toolCallId": tool_call_id,
                            "kind": "execute",
                            "status": "in_progress",
                            "content": [{"type": "terminal", "terminalId": terminal_id}],
                        },
                    }),
                );
            }
            Ok(json!({"terminalId": terminal_id}))
        }
        "clientTool/terminal/wait" => {
            let response = client.request("terminal/wait_for_exit", terminal()).await?;
            Ok(json!({
                "exitCode": response.get("exitCode").cloned().unwrap_or(Value::Null),
                "signal": response.get("signal").cloned().unwrap_or(Value::Null),
            }))
        }
        "clientTool/terminal/output" => {
            let response = client.request("terminal/output", terminal()).await?;
            Ok(json!({
                "output": response.get("output").cloned().unwrap_or(json!("")),
                "truncated": response.get("truncated").cloned().unwrap_or(json!(false)),
            }))
        }
        "clientTool/terminal/kill" => {
            client.request("terminal/kill", terminal()).await?;
            Ok(json!({}))
        }
        "clientTool/terminal/release" => {
            client.request("terminal/release", terminal()).await?;
            Ok(json!({}))
        }
        other => Err(format!("`{other}` is not a delegated tool")),
    }
}
