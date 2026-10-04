//! A scripted realtime transcription endpoint for tests: it accepts one
//! websocket, plays a script against it, and records what the client sent.

use std::net::SocketAddr;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};

/// One step of the endpoint's side of the conversation.
#[derive(Clone, Debug)]
pub(crate) enum EndpointStep {
    /// Sends a JSON frame.
    Send(Value),
    /// Reads client frames until one of this type arrives.
    ReadUntil(&'static str),
    /// Closes the connection, recording what the client sent before it saw
    /// the close.
    Close,
    /// Reads client frames until the client closes.
    AwaitClose,
}

/// What the client sent.
#[derive(Clone, Debug, Default)]
pub(crate) struct EndpointRecord {
    pub(crate) path: String,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) frames: Vec<Value>,
}

impl EndpointRecord {
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(candidate, _)| candidate.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    pub(crate) fn frame_types(&self) -> Vec<&str> {
        self.frames
            .iter()
            .filter_map(|frame| frame.get("type").and_then(Value::as_str))
            .collect()
    }
}

pub(crate) struct ScriptedEndpoint {
    address: SocketAddr,
    task: JoinHandle<EndpointRecord>,
}

impl ScriptedEndpoint {
    #[expect(
        clippy::result_large_err,
        reason = "the handshake callback's signature is tungstenite's"
    )]
    pub(crate) async fn start(steps: Vec<EndpointStep>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a local port");
        let address = listener.local_addr().expect("the bound address");
        let task = tokio::spawn(async move {
            let mut record = EndpointRecord::default();
            let Ok((stream, _)) = listener.accept().await else {
                return record;
            };
            let mut head = None;
            let callback = |request: &Request, response: Response| {
                head = Some((
                    request.uri().to_string(),
                    request
                        .headers()
                        .iter()
                        .map(|(name, value)| {
                            (
                                name.as_str().to_owned(),
                                value.to_str().unwrap_or_default().to_owned(),
                            )
                        })
                        .collect::<Vec<_>>(),
                ));
                Ok(response)
            };
            let Ok(mut socket) = tokio_tungstenite::accept_hdr_async(stream, callback).await else {
                return record;
            };
            if let Some((path, headers)) = head {
                record.path = path;
                record.headers = headers;
            }
            for step in steps {
                match step {
                    EndpointStep::Send(frame) => {
                        if socket
                            .send(Message::Text(frame.to_string().into()))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    EndpointStep::ReadUntil(kind) => loop {
                        let Some(Ok(message)) = socket.next().await else {
                            return record;
                        };
                        if let Some(frame) = crate::transcribe::message_json(&message) {
                            let done = frame.get("type").and_then(Value::as_str) == Some(kind);
                            record.frames.push(frame);
                            if done {
                                break;
                            }
                        }
                    },
                    EndpointStep::Close => {
                        let _ = socket.close(None).await;
                        while let Some(Ok(message)) = socket.next().await {
                            if let Some(frame) = crate::transcribe::message_json(&message) {
                                record.frames.push(frame);
                            }
                        }
                        return record;
                    }
                    EndpointStep::AwaitClose => {
                        while let Some(Ok(message)) = socket.next().await {
                            if let Some(frame) = crate::transcribe::message_json(&message) {
                                record.frames.push(frame);
                            }
                        }
                        return record;
                    }
                }
            }
            record
        });
        Self { address, task }
    }

    pub(crate) fn http_base(&self) -> String {
        format!("http://{}", self.address)
    }

    pub(crate) async fn finish(self) -> EndpointRecord {
        tokio::time::timeout(Duration::from_secs(5), self.task)
            .await
            .expect("the endpoint settles")
            .expect("the endpoint task")
    }
}
