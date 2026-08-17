//! End-to-end stdio transport tests for the ACP binary.
//!
//! This file holds the shared world the cases run against: a deterministic
//! authentication environment, a peer that speaks the wire, and the request
//! helpers every module reuses.

mod auth;
mod lifecycle;
mod transport;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{
    AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, ReadHalf, WriteHalf, duplex, split,
};
use tokio::task::JoinHandle;
use vibe_acp::{AcpAuthEnvironment, AuthAttemptFuture, AuthKeyFuture};
use vibe_app_server::client::TurnDriver;
use vibe_core::auth::{
    AuthState, AuthStateKind, PersistOutcome, RemoveError, SignInError, SignInErrorCode,
    default_mistral_provider,
};

use crate::stdio::{StdioOptions, run_stdio};

/// A deterministic authentication world for the wire tests: the shipped
/// provider, a scripted assessment, and no reachable sign-in transport.
struct StaticAuthEnvironment {
    state: Mutex<AuthState>,
}

impl StaticAuthEnvironment {
    fn new(state: AuthState) -> Self {
        Self {
            state: Mutex::new(state),
        }
    }

    fn signed_out() -> Self {
        Self::new(AuthState {
            kind: AuthStateKind::SignedOut,
            can_use_active_provider: false,
            sign_out_available: false,
            env_key: Some("MISTRAL_API_KEY".to_owned()),
        })
    }

    fn dotenv_backed() -> Self {
        Self::new(AuthState {
            kind: AuthStateKind::VibeHomeEnvFile,
            can_use_active_provider: true,
            sign_out_available: true,
            env_key: Some("MISTRAL_API_KEY".to_owned()),
        })
    }
}

#[allow(clippy::unwrap_in_result)]
impl AcpAuthEnvironment for StaticAuthEnvironment {
    fn load_provider(&self) -> toml::Table {
        default_mistral_provider()
    }

    fn assess(&self, _env_key: &str) -> std::io::Result<AuthState> {
        Ok(self.state.lock().expect("static auth state").clone())
    }

    fn persist_api_key(
        &self,
        _env_key: &str,
        _backend_is_mistral: bool,
        _api_key: &str,
        _custom_domain: bool,
    ) -> PersistOutcome {
        PersistOutcome::Completed
    }

    fn remove_api_key(&self, _env_key: &str) -> Result<(), RemoveError> {
        *self.state.lock().expect("static auth state") = AuthState {
            kind: AuthStateKind::SignedOut,
            can_use_active_provider: false,
            sign_out_available: false,
            env_key: Some("MISTRAL_API_KEY".to_owned()),
        };
        Ok(())
    }

    fn persist_provider(&self, _provider: &toml::Table) -> bool {
        true
    }

    fn browser_authenticate<'a>(&'a self, _provider: &'a toml::Table) -> AuthKeyFuture<'a> {
        Box::pin(async { Err(SignInError::new(SignInErrorCode::StartFailed)) })
    }

    fn start_attempt<'a>(&'a self, _provider: &'a toml::Table) -> AuthAttemptFuture<'a> {
        Box::pin(async { Err(SignInError::new(SignInErrorCode::StartFailed)) })
    }

    fn complete_attempt<'a>(
        &'a self,
        _provider: &'a toml::Table,
        _attempt: &'a vibe_core::auth::SignInAttempt,
    ) -> AuthKeyFuture<'a> {
        Box::pin(async { Err(SignInError::new(SignInErrorCode::StartFailed)) })
    }
}

struct TestPeer {
    reader: BufReader<ReadHalf<DuplexStream>>,
    writer: WriteHalf<DuplexStream>,
    server: JoinHandle<Result<(), String>>,
}

impl TestPeer {
    async fn send_raw(&mut self, frame: &[u8]) {
        self.writer.write_all(frame).await.expect("write frame");
        self.writer.write_all(b"\n").await.expect("write newline");
        self.writer.flush().await.expect("flush frame");
    }

    async fn send(&mut self, value: Value) {
        let frame = serde_json::to_vec(&value).expect("encode frame");
        self.send_raw(&frame).await;
    }

    async fn next(&mut self) -> Value {
        let mut line = String::new();
        let bytes = tokio::time::timeout(Duration::from_secs(2), self.reader.read_line(&mut line))
            .await
            .expect("server response timeout")
            .expect("read server response");
        assert!(bytes > 0, "server closed before sending a response");
        serde_json::from_str(&line).expect("decode server response")
    }

    async fn response(&mut self, id: i64) -> (Vec<Value>, Value) {
        let mut preceding = Vec::new();
        loop {
            let message = self.next().await;
            if message.get("id").and_then(Value::as_i64) == Some(id) {
                return (preceding, message);
            }
            preceding.push(message);
        }
    }

    /// Consumes the `available_commands_update` that follows a session
    /// lifecycle response, and asserts it arrives after that response.
    ///
    /// Reference `_create_session` spawns `_send_initial_commands`, which
    /// sleeps before publishing, so a client is always told a session exists
    /// before it is told what that session can run.
    async fn expect_commands_update(&mut self, session_id: &str) {
        let message = self.next().await;
        assert_eq!(message["method"], "session/update", "{message}");
        assert_eq!(message["params"]["sessionId"], session_id, "{message}");
        assert_eq!(
            message["params"]["update"]["sessionUpdate"], "available_commands_update",
            "{message}"
        );
    }

    async fn shutdown(mut self, id: i64) {
        self.send(request(id, "shutdown", json!({}))).await;
        let (_, response) = self.response(id).await;
        assert_eq!(response["result"], json!({}));
        self.server
            .await
            .expect("stdio server task")
            .expect("stdio server shutdown");
    }
}

fn spawn_stdio<D>(driver: D) -> TestPeer
where
    D: TurnDriver + 'static,
{
    spawn_stdio_with_root(driver, None)
}

fn spawn_stdio_with_root<D>(driver: D, session_root: Option<PathBuf>) -> TestPeer
where
    D: TurnDriver + 'static,
{
    spawn_stdio_with_auth(
        driver,
        session_root,
        Arc::new(StaticAuthEnvironment::signed_out()),
    )
}

fn spawn_stdio_with_auth<D>(
    driver: D,
    session_root: Option<PathBuf>,
    auth_environment: Arc<dyn AcpAuthEnvironment>,
) -> TestPeer
where
    D: TurnDriver + 'static,
{
    let (server_io, client_io) = duplex(256 * 1024);
    let (server_reader, server_writer) = split(server_io);
    let (client_reader, client_writer) = split(client_io);
    let server = tokio::spawn(async move {
        run_stdio(
            BufReader::new(server_reader),
            server_writer,
            driver,
            StdioOptions {
                experiments: None,
                session_root,
                credential_environment: "MISTRAL_API_KEY".to_owned(),
                auth_environment,
                production_cloud: false,
                telemetry: None,
            },
        )
        .await
        .map_err(|error| error.to_string())
    });
    TestPeer {
        reader: BufReader::new(client_reader),
        writer: client_writer,
        server,
    }
}

fn request(id: i64, method: &str, params: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    })
}

async fn initialize(peer: &mut TestPeer, id: i64) -> Value {
    peer.send(request(
        id,
        "initialize",
        json!({"protocolVersion": vibe_acp::ACP_PROTOCOL_VERSION}),
    ))
    .await;
    let (_, response) = peer.response(id).await;
    assert!(response.get("error").is_none(), "{response}");
    response
}

async fn new_session(peer: &mut TestPeer, id: i64, cwd: &str) -> String {
    peer.send(request(id, "session/new", json!({"cwd": cwd})))
        .await;
    let (preceding, response) = peer.response(id).await;
    assert!(
        preceding.is_empty(),
        "session/new was preceded by {preceding:?}"
    );
    assert!(response.get("error").is_none(), "{response}");
    let session_id = response
        .pointer("/result/sessionId")
        .and_then(Value::as_str)
        .expect("session ID")
        .to_owned();
    peer.expect_commands_update(&session_id).await;
    session_id
}

async fn prompt(peer: &mut TestPeer, id: i64, session_id: &str, text: &str) -> (Vec<Value>, Value) {
    peer.send(request(
        id,
        "session/prompt",
        json!({
            "sessionId": session_id,
            "prompt": [{"type": "text", "text": text}],
        }),
    ))
    .await;
    peer.response(id).await
}

async fn close_session(peer: &mut TestPeer, id: i64, session_id: &str) -> Value {
    peer.send(request(
        id,
        "session/close",
        json!({"sessionId": session_id}),
    ))
    .await;
    let (_, response) = peer.response(id).await;
    response
}
