//! The writer half of the transport, and the client port agent-initiated
//! requests and notifications travel out through.
//!
//! Every frame goes through one unbounded queue, so a notification the agent
//! produced before a response reaches the editor before it, which is the
//! order the reference's single connection writes in.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicI64, Ordering};

use serde_json::{Value, json};
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};
use vibe_acp::{AcpClientFuture, AcpClientPort};

pub(crate) enum WriterMessage {
    Value(Value),
    Shutdown(oneshot::Sender<()>),
}

type Pending = Mutex<BTreeMap<i64, oneshot::Sender<Result<Value, String>>>>;

pub(crate) struct StdioClientPort {
    writer: mpsc::UnboundedSender<WriterMessage>,
    pending: Pending,
    next_id: AtomicI64,
}

impl StdioClientPort {
    pub(crate) fn new(writer: mpsc::UnboundedSender<WriterMessage>) -> Self {
        Self {
            writer,
            pending: Mutex::new(BTreeMap::new()),
            next_id: AtomicI64::new(0),
        }
    }

    /// Hands a response the client sent to the request waiting on it. A
    /// response nothing waits on is dropped.
    pub(crate) fn resolve(&self, value: &Value) {
        let Some(id) = value.get("id").and_then(Value::as_i64) else {
            return;
        };
        let sender = self
            .pending
            .lock()
            .ok()
            .and_then(|mut pending| pending.remove(&id));
        let Some(sender) = sender else {
            return;
        };
        let result = match value.get("result") {
            Some(result) => Ok(result.clone()),
            None => Err(value.get("error").map_or_else(
                || "the client answered without a result".to_owned(),
                Value::to_string,
            )),
        };
        let _ = sender.send(result);
    }

    /// Fails every request still waiting on the client.
    pub(crate) fn disconnect(&self) {
        if let Ok(mut pending) = self.pending.lock() {
            for (_, sender) in std::mem::take(&mut *pending) {
                let _ = sender.send(Err("the client disconnected".to_owned()));
            }
        }
    }

    pub(crate) fn send(&self, value: Value) {
        let _ = self.writer.send(WriterMessage::Value(value));
    }
}

/// Forgets a pending request whose caller stopped waiting.
struct PendingGuard<'a> {
    pending: &'a Pending,
    id: i64,
}

impl Drop for PendingGuard<'_> {
    fn drop(&mut self) {
        if let Ok(mut pending) = self.pending.lock() {
            pending.remove(&self.id);
        }
    }
}

impl AcpClientPort for StdioClientPort {
    fn request<'a>(&'a self, method: &'a str, params: Value) -> AcpClientFuture<'a> {
        Box::pin(async move {
            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
            let (sender, receiver) = oneshot::channel();
            self.pending
                .lock()
                .map_err(|_| "the client request table is poisoned".to_owned())?
                .insert(id, sender);
            let _guard = PendingGuard {
                pending: &self.pending,
                id,
            };
            self.send(json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": method,
                "params": params,
            }));
            receiver
                .await
                .map_err(|_| "the client response channel closed".to_owned())?
        })
    }

    fn notify(&self, method: &str, params: Value) {
        self.send(json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }
}

pub(crate) async fn writer_loop<W>(
    mut writer: W,
    mut receiver: mpsc::UnboundedReceiver<WriterMessage>,
) -> Result<(), std::io::Error>
where
    W: AsyncWrite + Unpin,
{
    while let Some(message) = receiver.recv().await {
        match message {
            WriterMessage::Value(value) => {
                let mut bytes = serde_json::to_vec(&value).map_err(std::io::Error::other)?;
                bytes.push(b'\n');
                writer.write_all(&bytes).await?;
                writer.flush().await?;
            }
            WriterMessage::Shutdown(acknowledge) => {
                writer.flush().await?;
                let _ = acknowledge.send(());
                break;
            }
        }
    }
    Ok(())
}
