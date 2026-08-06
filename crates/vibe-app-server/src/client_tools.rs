//! Server-to-client tool delegation.
//!
//! A client that hosts an editor can answer for the filesystem and the terminal
//! on the agent's behalf, so the file a tool reads is the buffer the user is
//! looking at and the command it runs appears in their terminal rather than in a
//! hidden process. The reference declares seven `clientTool/*` methods for this,
//! all issued as server-to-client requests and all gated on the capabilities the
//! client declared during `initialize`.
//!
//! This module is the transport half. [`ClientToolBridge`] serializes a
//! [`ClientToolRequest`] into a JSON-RPC request frame, hands it to whoever owns
//! the write side of the connection, and parks the caller until the client's
//! answer comes back through [`ClientToolBridge::resolve`]. The lifecycle half,
//! including the terminal ordering and the deadlines, belongs to
//! `vibe_core::process::ClientToolIo`.

use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};

use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use vibe_core::process::{
    ClientToolCapability, ClientToolIo, ClientToolPort, ClientToolRequest, ToolIoError,
    ToolIoFuture,
};
use vibe_protocol::{
    ClientToolCapability as WireCapability, Envelope, JsonRpcVersion, RequestId, ServerRequest,
    encode_frame,
};

/// What one connection lets its server delegate, and where the delegation goes.
#[derive(Default)]
struct BridgeState {
    capabilities: BTreeSet<ClientToolCapability>,
    /// The write side of the live connection. Absent before a transport claims
    /// it, which is what makes an in-process client delegate nothing.
    outbound: Option<mpsc::UnboundedSender<Vec<u8>>>,
    pending: HashMap<RequestId, oneshot::Sender<Result<Value, String>>>,
    /// The delegation each open session's tools were published against, kept so
    /// a terminal the client still holds for a closing session is released
    /// rather than left to it.
    sessions: HashMap<String, ClientToolIo>,
}

/// The `clientTool/*` requests one connection has in flight.
///
/// It is owned by the server rather than by the connection because the tools
/// that delegate run on spawned turn tasks, while the answers arrive on the
/// connection's own read loop. Both reach the same registry through the shared
/// server handle.
#[derive(Default)]
pub struct ClientToolBridge {
    state: Mutex<BridgeState>,
    next_request: AtomicU64,
}

impl ClientToolBridge {
    /// Records what the handshake declared. Called on every `initialize`, so a
    /// reconnection that declares less is honored immediately.
    pub fn declare(&self, capabilities: &[WireCapability]) {
        let declared = capabilities
            .iter()
            .map(|capability| match capability {
                WireCapability::FilesystemRead => ClientToolCapability::FilesystemRead,
                WireCapability::FilesystemWrite => ClientToolCapability::FilesystemWrite,
                WireCapability::Terminal => ClientToolCapability::Terminal,
            })
            .collect();
        self.lock().capabilities = declared;
    }

    /// Claims the write side of the connection, which is what a delegated
    /// request travels on.
    pub fn attach(&self, outbound: mpsc::UnboundedSender<Vec<u8>>) {
        self.lock().outbound = Some(outbound);
    }

    /// Drops the connection and fails every request still waiting on it, so a
    /// tool blocked on a client that went away reports the loss rather than
    /// waiting out its deadline.
    pub fn detach(&self) {
        let mut state = self.lock();
        state.outbound = None;
        state.capabilities.clear();
        state.sessions.clear();
        for (id, waiting) in std::mem::take(&mut state.pending) {
            let _ = waiting.send(Err(format!(
                "the client disconnected before answering request {id:?}"
            )));
        }
    }

    /// Routes one client answer back to the request that is waiting for it, and
    /// reports whether the id was one of ours.
    ///
    /// The connection asks this before its callback registry: an id neither
    /// owns is a protocol fault, and only the caller can tell the difference.
    pub fn resolve(&self, id: &RequestId, result: Result<Value, String>) -> bool {
        let Some(waiting) = self.lock().pending.remove(id) else {
            return false;
        };
        let _ = waiting.send(result);
        true
    }

    /// The delegation one session's tools use, or `None` when the client hosts
    /// nothing for us to delegate to.
    ///
    /// A session republishing its tools reuses the delegation it already has,
    /// so the terminals it opened stay tracked across the refresh.
    ///
    /// A connection with no write side hosts nothing whatever it declared: the
    /// ACP bridge names the capabilities its editor answers through its own
    /// tool family and no `clientTool/*` frame can leave an in-process
    /// connection, so its tools stay on this host rather than failing on a
    /// request nothing carries.
    #[must_use]
    pub fn session_io(self: &std::sync::Arc<Self>, session_id: &str) -> Option<ClientToolIo> {
        let mut state = self.lock();
        if state.capabilities.is_empty() || state.outbound.is_none() {
            return None;
        }
        Some(
            state
                .sessions
                .entry(session_id.to_owned())
                .or_insert_with(|| ClientToolIo::new(session_id, self.clone()))
                .clone(),
        )
    }

    /// Releases every client terminal a closing session still holds.
    ///
    /// A client terminal outlives the tool call that created it only when that
    /// call was abandoned, and nothing else can close one once the session is
    /// gone.
    pub async fn close_session(&self, session_id: &str) -> Result<(), ToolIoError> {
        let Some(session) = self.lock().sessions.remove(session_id) else {
            return Ok(());
        };
        session.cleanup_all().await
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BridgeState> {
        // A poisoned registry still has to answer: the alternative is a turn
        // that hangs on a delegation nobody will ever resolve.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Puts one request on the wire and registers where its answer goes.
    fn dispatch(
        &self,
        request: &ClientToolRequest,
    ) -> Result<oneshot::Receiver<Result<Value, String>>, ToolIoError> {
        let params = match serde_json::to_value(request) {
            Ok(Value::Object(envelope)) => envelope
                .get("params")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default(),
            _ => {
                return Err(ToolIoError::Request(
                    "request is not serializable".to_owned(),
                ));
            }
        };
        let sequence = self.next_request.fetch_add(1, Ordering::Relaxed);
        // A string id keeps this counter out of the callback registry's integer
        // space, so the two server-to-client families can never collide.
        let id = RequestId::String(format!("clientTool-{sequence}"));
        let frame = encode_frame(&Envelope::Request(ServerRequest {
            jsonrpc: JsonRpcVersion::V2,
            id: id.clone(),
            method: request.method().to_owned(),
            params: params.into_iter().collect(),
        }));
        let (sender, receiver) = oneshot::channel();
        let mut state = self.lock();
        let Some(outbound) = state.outbound.clone() else {
            return Err(ToolIoError::Request(
                "no client is connected to delegate to".to_owned(),
            ));
        };
        state.pending.insert(id.clone(), sender);
        drop(state);
        if outbound.send(frame).is_err() {
            self.lock().pending.remove(&id);
            return Err(ToolIoError::Request(
                "the client transport is closed".to_owned(),
            ));
        }
        Ok(receiver)
    }
}

impl ClientToolPort for ClientToolBridge {
    fn request<'a>(&'a self, request: ClientToolRequest) -> ToolIoFuture<'a> {
        Box::pin(async move {
            let receiver = self.dispatch(&request)?;
            match receiver.await {
                Ok(Ok(result)) => Ok(result),
                Ok(Err(message)) => Err(ToolIoError::Request(message)),
                Err(_) => Err(ToolIoError::Request(
                    "the delegation was dropped before the client answered".to_owned(),
                )),
            }
        })
    }

    fn supports(&self, capability: ClientToolCapability) -> bool {
        self.lock().capabilities.contains(&capability)
    }
}

#[cfg(test)]
mod tests;
