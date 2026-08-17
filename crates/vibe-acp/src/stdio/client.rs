//! The writer half of the transport, and the client port agent-initiated
//! requests travel out through.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicI64, Ordering};

use serde_json::{Value, json};
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};
use vibe_acp::{AcpClientFuture, AcpClientPort};
use vibe_app_server::transport::MAX_FRAME_BYTES;

pub(crate) const WRITER_QUEUE_CAPACITY: usize = 1_024;
pub(crate) const MAX_PENDING_CLIENT_REQUESTS: usize = 256;

pub(crate) enum WriterMessage {
    Value(Value),
    Shutdown(oneshot::Sender<()>),
}

pub(crate) struct StdioClientPort {
    writer: mpsc::Sender<WriterMessage>,
    pub(crate) pending: Mutex<BTreeMap<i64, oneshot::Sender<Result<Value, String>>>>,
    next_id: AtomicI64,
}

impl StdioClientPort {
    pub(crate) fn new(writer: mpsc::Sender<WriterMessage>) -> Self {
        Self {
            writer,
            pending: Mutex::new(BTreeMap::new()),
            next_id: AtomicI64::new(1_000_000),
        }
    }

    pub(crate) fn resolve(&self, value: &Value) -> bool {
        let Some(id) = value.get("id").and_then(Value::as_i64) else {
            return false;
        };
        let sender = self
            .pending
            .lock()
            .ok()
            .and_then(|mut pending| pending.remove(&id));
        let Some(sender) = sender else {
            return false;
        };
        let result = value.get("result").cloned().ok_or_else(|| {
            value
                .pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or("ACP client returned an invalid error")
                .to_owned()
        });
        let _ = sender.send(result);
        true
    }

    pub(crate) fn disconnect(&self) {
        if let Ok(mut pending) = self.pending.lock() {
            for (_, sender) in std::mem::take(&mut *pending) {
                let _ = sender.send(Err("ACP client disconnected".to_owned()));
            }
        }
    }
}

struct PendingRequestGuard<'a> {
    pending: &'a Mutex<BTreeMap<i64, oneshot::Sender<Result<Value, String>>>>,
    id: i64,
    armed: bool,
}

impl PendingRequestGuard<'_> {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for PendingRequestGuard<'_> {
    fn drop(&mut self) {
        if self.armed
            && let Ok(mut pending) = self.pending.lock()
        {
            pending.remove(&self.id);
        }
    }
}

impl AcpClientPort for StdioClientPort {
    fn request<'a>(&'a self, method: &'a str, params: Value) -> AcpClientFuture<'a> {
        Box::pin(async move {
            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
            let (sender, receiver) = oneshot::channel();
            {
                let mut pending = self
                    .pending
                    .lock()
                    .map_err(|_| "ACP client request lock is poisoned".to_owned())?;
                if pending.len() >= MAX_PENDING_CLIENT_REQUESTS {
                    return Err("too many pending ACP client requests".to_owned());
                }
                pending.insert(id, sender);
            }
            let mut guard = PendingRequestGuard {
                pending: &self.pending,
                id,
                armed: true,
            };
            if self
                .writer
                .send(WriterMessage::Value(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "method": method,
                    "params": params,
                })))
                .await
                .is_err()
            {
                return Err("ACP writer is closed".to_owned());
            }
            let response = receiver
                .await
                .map_err(|_| "ACP client response channel closed".to_owned())?;
            guard.disarm();
            response
        })
    }
}

pub(crate) async fn writer_loop<W>(
    mut writer: W,
    mut receiver: mpsc::Receiver<WriterMessage>,
) -> Result<(), std::io::Error>
where
    W: AsyncWrite + Unpin,
{
    while let Some(message) = receiver.recv().await {
        match message {
            WriterMessage::Value(value) => {
                let mut bytes = serde_json::to_vec(&value).map_err(std::io::Error::other)?;
                if bytes.len() > MAX_FRAME_BYTES {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!(
                            "ACP output frame exceeds the {MAX_FRAME_BYTES}-byte transport limit"
                        ),
                    ));
                }
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

pub(crate) fn send_value(writer: &mpsc::Sender<WriterMessage>, value: Value) -> Result<(), String> {
    writer
        .try_send(WriterMessage::Value(value))
        .map_err(|error| format!("ACP writer queue is unavailable: {error}"))
}

pub(crate) async fn send_value_wait(
    writer: &mpsc::Sender<WriterMessage>,
    value: Value,
) -> Result<(), String> {
    writer
        .send(WriterMessage::Value(value))
        .await
        .map_err(|_| "ACP writer queue is closed".to_owned())
}
