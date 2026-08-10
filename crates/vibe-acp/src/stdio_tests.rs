//! End-to-end stdio transport tests for the ACP binary.

use std::sync::atomic::AtomicUsize;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, DuplexStream, ReadHalf, WriteHalf, duplex, split};
use tokio::task::JoinHandle;
use vibe_acp::{AuthAttemptFuture, AuthKeyFuture};
use vibe_app_server::client::{EchoTurnDriver, TurnReservation};
use vibe_core::auth::{
    AuthState, AuthStateKind, PersistOutcome, RemoveError, SignInError, SignInErrorCode,
    default_mistral_provider,
};
use vibe_core::compaction::manager::CompactionPromptResolution;

use super::*;

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
            session_root,
            "MISTRAL_API_KEY".to_owned(),
            auth_environment,
            false,
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
    let (_, response) = peer.response(id).await;
    assert!(response.get("error").is_none(), "{response}");
    response
        .pointer("/result/sessionId")
        .and_then(Value::as_str)
        .expect("session ID")
        .to_owned()
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

#[test]
fn writer_queue_reports_saturation_without_allocating_past_its_bound() {
    let (writer, _receiver) = mpsc::channel(1);
    send_value(&writer, json!({"first": true})).expect("first value fits");
    let error =
        send_value(&writer, json!({"second": true})).expect_err("second value exceeds the bound");
    assert!(error.contains("no available capacity"), "{error}");
}

#[tokio::test]
async fn client_request_limit_and_disconnect_fail_closed() {
    let (writer, mut receiver) = mpsc::channel(1);
    let client = Arc::new(StdioClientPort::new(writer));
    {
        let mut pending = client.pending.lock().expect("pending lock");
        for id in 0..MAX_PENDING_CLIENT_REQUESTS {
            let (sender, _receiver) = oneshot::channel();
            pending.insert(i64::try_from(id).expect("bounded ID"), sender);
        }
    }
    let error = client
        .request("session/request_permission", json!({}))
        .await
        .expect_err("pending request limit is enforced");
    assert!(error.contains("too many pending"), "{error}");

    client.pending.lock().expect("pending lock").clear();
    let request_client = client.clone();
    let request = tokio::spawn(async move {
        request_client
            .request("session/request_permission", json!({}))
            .await
    });
    assert!(matches!(
        receiver.recv().await,
        Some(WriterMessage::Value(_))
    ));
    client.disconnect();
    let error = request
        .await
        .expect("request task")
        .expect_err("disconnect rejects pending request");
    assert!(error.contains("disconnected"), "{error}");
    assert!(client.pending.lock().expect("pending lock").is_empty());
}

#[tokio::test]
async fn malformed_frames_return_protocol_errors_without_stopping_the_loop() {
    let mut peer = spawn_stdio(EchoTurnDriver::new("answer"));

    peer.send_raw(b"{not-json").await;
    let parse_error = peer.next().await;
    assert!(parse_error["id"].is_null());
    assert_eq!(parse_error["error"]["code"], -32700);

    peer.send_raw(br#"{"jsonrpc":"2.0","id":2,"method":7}"#)
        .await;
    let request_error = peer.next().await;
    assert!(request_error["id"].is_null());
    assert_eq!(request_error["error"]["code"], -32600);

    let initialized = initialize(&mut peer, 3).await;
    assert_eq!(
        initialized["result"]["protocolVersion"],
        vibe_acp::ACP_PROTOCOL_VERSION
    );
    peer.shutdown(4).await;
}

#[tokio::test]
async fn initialize_and_session_lifecycle_do_not_require_provider_credentials() {
    const MISSING_CREDENTIAL: &str = "VIBE_ACP_TEST_CREDENTIAL_MUST_REMAIN_UNSET_9F4C";
    assert!(
        std::env::var_os(MISSING_CREDENTIAL).is_none(),
        "{MISSING_CREDENTIAL} must remain unset for this test"
    );
    let resolutions = Arc::new(AtomicUsize::new(0));
    let driver = DeferredTurnDriver::<LiveTurnDriver>::new({
        let resolutions = resolutions.clone();
        move || {
            resolutions.fetch_add(1, Ordering::SeqCst);
            LiveTurnDriver::from_environment(
                LiveDriverConfig {
                    style: "mistral".to_owned(),
                    endpoint: "http://127.0.0.1:1".to_owned(),
                    model: "test-model".to_owned(),
                    credential_environment: MISSING_CREDENTIAL.to_owned(),
                    system_prompt: "test".to_owned(),
                    session_root: None,
                    compaction_prompts: CompactionPromptResolution::default(),
                    input_price_per_million_micros: 0,
                    output_price_per_million_micros: 0,
                },
                &DotenvValues::default(),
            )
        }
    });
    let mut peer = spawn_stdio(driver);

    let initialized = initialize(&mut peer, 1).await;
    assert_eq!(
        initialized["result"]["authMethods"][0]["id"],
        "browser-auth"
    );
    let session_id = new_session(&mut peer, 2, "/workspace").await;
    let closed = close_session(&mut peer, 3, &session_id).await;
    assert_eq!(closed["result"], json!({}));
    assert_eq!(resolutions.load(Ordering::SeqCst), 0);

    let session_id = new_session(&mut peer, 4, "/workspace").await;
    let (_, response) = prompt(&mut peer, 5, &session_id, "requires provider").await;
    assert_eq!(response["error"]["code"], -32603);
    assert!(
        response["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains(MISSING_CREDENTIAL))
    );
    assert_eq!(resolutions.load(Ordering::SeqCst), 1);
    peer.shutdown(6).await;
}

#[tokio::test]
async fn initialize_precedes_provider_use_and_prompt_drains_the_final_update() {
    let resolutions = Arc::new(AtomicUsize::new(0));
    let driver = DeferredTurnDriver::new({
        let resolutions = resolutions.clone();
        move || {
            resolutions.fetch_add(1, Ordering::SeqCst);
            Ok(EchoTurnDriver::new("answer"))
        }
    });
    let mut peer = spawn_stdio(driver);

    initialize(&mut peer, 1).await;
    let session_id = new_session(&mut peer, 2, "/workspace").await;
    assert_eq!(resolutions.load(Ordering::SeqCst), 0);

    let (preceding, response) = prompt(&mut peer, 3, &session_id, "question").await;
    assert_eq!(response["result"]["stopReason"], "end_turn");
    assert_eq!(resolutions.load(Ordering::SeqCst), 1);
    assert_eq!(
        preceding
            .last()
            .and_then(|message| message.pointer("/params/update/sessionUpdate"))
            .and_then(Value::as_str),
        Some("usage_update")
    );
    peer.shutdown(4).await;
}

#[tokio::test]
async fn stdio_session_list_honors_cwd_and_cursor_with_acp_session_shapes() {
    const PAGE_SIZE: usize = 50;
    let temporary = tempfile::tempdir().expect("temporary root");
    let first_cwd = temporary.path().join("first");
    let second_cwd = temporary.path().join("second");
    std::fs::create_dir_all(&first_cwd).expect("first cwd");
    std::fs::create_dir_all(&second_cwd).expect("second cwd");
    let mut peer = spawn_stdio_with_root(
        EchoTurnDriver::new("answer"),
        Some(temporary.path().join("sessions")),
    );
    initialize(&mut peer, 1).await;

    for id in 0..=PAGE_SIZE {
        new_session(
            &mut peer,
            i64::try_from(id).unwrap_or_default().saturating_add(10),
            &first_cwd.to_string_lossy(),
        )
        .await;
    }
    new_session(&mut peer, 100, &second_cwd.to_string_lossy()).await;

    peer.send(request(
        101,
        "session/list",
        json!({"cwd": first_cwd.to_string_lossy()}),
    ))
    .await;
    let (_, first) = peer.response(101).await;
    let first_sessions = first["result"]["sessions"].as_array().expect("sessions");
    assert_eq!(first_sessions.len(), PAGE_SIZE);
    assert!(first_sessions.iter().all(|session| {
        session.get("sessionId").is_some_and(Value::is_string)
            && session["cwd"] == first_cwd.to_string_lossy().as_ref()
            && session.get("id").is_none()
            && session.get("workingDirectory").is_none()
    }));
    let cursor = first["result"]["nextCursor"].as_str().expect("next cursor");

    peer.send(request(
        102,
        "session/list",
        json!({"cwd": first_cwd.to_string_lossy(), "cursor": cursor}),
    ))
    .await;
    let (_, second) = peer.response(102).await;
    assert_eq!(
        second["result"]["sessions"].as_array().map(Vec::len),
        Some(1)
    );
    assert!(second["result"].get("nextCursor").is_none());
    let first_ids = first_sessions
        .iter()
        .filter_map(|session| session["sessionId"].as_str())
        .collect::<std::collections::BTreeSet<_>>();
    let second_id = second["result"]["sessions"][0]["sessionId"]
        .as_str()
        .expect("second-page ID");
    assert!(!first_ids.contains(second_id));

    peer.send(request(
        103,
        "session/list",
        json!({"cwd": second_cwd.to_string_lossy()}),
    ))
    .await;
    let (_, filtered) = peer.response(103).await;
    assert_eq!(
        filtered["result"]["sessions"][0]["cwd"],
        second_cwd.to_string_lossy().as_ref()
    );
    peer.shutdown(104).await;
}

#[tokio::test]
async fn stdio_load_and_fork_preserve_structured_replay_and_lifecycle_options() {
    let temporary = tempfile::tempdir().expect("temporary root");
    let cwd = temporary.path().join("workspace");
    let additional = temporary.path().join("shared");
    std::fs::create_dir_all(&cwd).expect("cwd");
    std::fs::create_dir_all(&additional).expect("additional root");
    let session_root = temporary.path().join("sessions");
    let mut peer = spawn_stdio_with_root(
        EchoTurnDriver::new("answer").with_session_root(session_root.clone()),
        Some(session_root),
    );
    initialize(&mut peer, 1).await;
    let source = new_session(&mut peer, 2, &cwd.to_string_lossy()).await;
    let (_, prompted) = prompt(&mut peer, 3, &source, "question").await;
    assert_eq!(prompted["result"]["stopReason"], "end_turn");

    peer.send(request(
        4,
        "session/set_mode",
        json!({"sessionId": source, "modeId": "plan"}),
    ))
    .await;
    assert!(peer.response(4).await.1.get("error").is_none());
    peer.send(request(
        5,
        "session/set_config_option",
        json!({"sessionId": source, "configId": "thinking", "value": "high"}),
    ))
    .await;
    assert!(peer.response(5).await.1.get("error").is_none());

    let mcp_server = json!({
        "type": "stdio",
        "name": "fixture",
        "command": "/bin/false",
        "args": [],
        "env": [],
    });
    peer.send(request(
        6,
        "session/fork",
        json!({
            "sessionId": source,
            "cwd": cwd.to_string_lossy(),
            "additionalDirectories": [additional.to_string_lossy()],
            "mcpServers": [mcp_server],
            "newSessionId": "stdio-fork",
        }),
    ))
    .await;
    let (_, forked) = peer.response(6).await;
    assert_eq!(forked["result"]["sessionId"], "stdio-fork");
    assert_eq!(forked["result"]["modes"]["currentModeId"], "plan");
    assert_eq!(forked["result"]["configOptions"][0]["currentValue"], "high");

    peer.send(request(
        7,
        "session/fork",
        json!({
            "sessionId": source,
            "cwd": cwd.to_string_lossy(),
            "messageId": "message-1",
        }),
    ))
    .await;
    let (_, unsupported_fork) = peer.response(7).await;
    assert_eq!(unsupported_fork["error"]["code"], -32602);

    assert_eq!(
        close_session(&mut peer, 8, "stdio-fork").await["result"],
        json!({})
    );
    assert_eq!(
        close_session(&mut peer, 9, &source).await["result"],
        json!({})
    );
    peer.send(request(
        10,
        "session/load",
        json!({
            "sessionId": source,
            "cwd": cwd.to_string_lossy(),
            "additionalDirectories": [additional.to_string_lossy()],
            "mcpServers": [{
                "type": "stdio",
                "name": "fixture",
                "command": "/bin/false",
                "args": [],
                "env": [],
            }],
        }),
    ))
    .await;
    let (replay, loaded) = peer.response(10).await;
    assert!(loaded.get("error").is_none(), "{loaded}");
    assert!(loaded["result"].get("sessionId").is_none());
    let replay_updates = replay
        .iter()
        .filter_map(|message| message.pointer("/params/update"))
        .collect::<Vec<_>>();
    assert!(
        replay_updates.iter().any(|update| {
            update["sessionUpdate"] == "user_message_chunk"
                && update["content"] == json!({"type": "text", "text": "question"})
                && update.get("messageId").is_some()
        }),
        "{replay_updates:#?}"
    );
    assert!(replay_updates.iter().any(|update| {
        update["sessionUpdate"] == "agent_message_chunk"
            && update["content"] == json!({"type": "text", "text": "answer"})
            && update.get("messageId").is_some()
    }));
    assert!(replay_updates.iter().all(|update| {
        update["sessionUpdate"] != "agent_message_chunk" || update["content"]["text"].is_string()
    }));
    assert_eq!(
        close_session(&mut peer, 11, &source).await["result"],
        json!({})
    );
    peer.shutdown(12).await;
}

#[derive(Debug, PartialEq, Eq)]
struct RecordedTurn {
    session_id: String,
    prompt: String,
    working_directory: String,
}

struct RecordingDriver {
    inner: EchoTurnDriver,
    turns: Arc<Mutex<Vec<RecordedTurn>>>,
}

impl TurnDriver for RecordingDriver {
    fn run<'a>(&'a self, reservation: &'a TurnReservation) -> DriverFuture<'a> {
        self.turns.lock().expect("turns").push(RecordedTurn {
            session_id: reservation.session_id.clone(),
            prompt: reservation.prompt.clone(),
            working_directory: reservation.working_directory.clone(),
        });
        self.inner.run(reservation)
    }
}

#[tokio::test]
async fn two_stdio_session_lifecycles_remain_isolated() {
    let turns = Arc::new(Mutex::new(Vec::new()));
    let driver = RecordingDriver {
        inner: EchoTurnDriver::new("answer"),
        turns: turns.clone(),
    };
    let mut peer = spawn_stdio(driver);

    initialize(&mut peer, 1).await;
    let first = new_session(&mut peer, 2, "/first").await;
    let second = new_session(&mut peer, 3, "/second").await;
    assert_ne!(first, second);

    let (first_updates, first_response) = prompt(&mut peer, 4, &first, "first").await;
    assert!(first_response.get("error").is_none());
    assert!(first_updates.iter().all(|message| {
        message.get("method").and_then(Value::as_str) != Some("session/update")
            || message.pointer("/params/sessionId").and_then(Value::as_str) == Some(first.as_str())
    }));

    let (second_updates, second_response) = prompt(&mut peer, 5, &second, "second").await;
    assert!(second_response.get("error").is_none());
    assert!(second_updates.iter().all(|message| {
        message.get("method").and_then(Value::as_str) != Some("session/update")
            || message.pointer("/params/sessionId").and_then(Value::as_str) == Some(second.as_str())
    }));

    let closed = close_session(&mut peer, 6, &first).await;
    assert_eq!(closed["result"], json!({}));
    let (_, still_active) = prompt(&mut peer, 7, &second, "still active").await;
    assert!(still_active.get("error").is_none());
    let (_, closed_error) = prompt(&mut peer, 8, &first, "closed").await;
    assert_eq!(closed_error["error"]["code"], -32001);

    assert_eq!(
        turns.lock().expect("turns").as_slice(),
        [
            RecordedTurn {
                session_id: first,
                prompt: "first".to_owned(),
                working_directory: "/first".to_owned(),
            },
            RecordedTurn {
                session_id: second.clone(),
                prompt: "second".to_owned(),
                working_directory: "/second".to_owned(),
            },
            RecordedTurn {
                session_id: second.clone(),
                prompt: "still active".to_owned(),
                working_directory: "/second".to_owned(),
            },
        ]
    );
    let closed = close_session(&mut peer, 9, &second).await;
    assert_eq!(closed["result"], json!({}));
    peer.shutdown(10).await;
}

#[tokio::test]
async fn the_advertised_auth_methods_follow_the_client_capability_gates() {
    let mut peer = spawn_stdio(EchoTurnDriver::new("ok"));
    peer.send(request(
        1,
        "initialize",
        json!({
            "protocolVersion": vibe_acp::ACP_PROTOCOL_VERSION,
            "clientCapabilities": {
                "_meta": {"browser-auth-delegated": true, "terminal-auth": true},
            },
        }),
    ))
    .await;
    let (_, response) = peer.response(1).await;
    let methods = response["result"]["authMethods"]
        .as_array()
        .expect("auth methods")
        .iter()
        .map(|method| method["id"].as_str().expect("method id").to_owned())
        .collect::<Vec<_>>();
    assert_eq!(
        methods,
        ["browser-auth", "browser-auth-delegated", "vibe-setup"]
    );
    let terminal = &response["result"]["authMethods"][2];
    assert_eq!(terminal["args"], json!(["--setup"]));
    let command = terminal["_meta"]["terminal-auth"]["command"]
        .as_str()
        .expect("terminal command");
    assert_eq!(
        std::path::Path::new(command).file_stem(),
        Some(std::ffi::OsStr::new("vibe")),
        "{command}"
    );

    // The reference refuses every id `authenticate` does not serve itself,
    // including the terminal id and this port's former `environment` method.
    peer.send(request(
        2,
        "authenticate",
        json!({"methodId": "environment"}),
    ))
    .await;
    let (_, refused) = peer.response(2).await;
    assert_eq!(refused["error"]["code"], -32602);
    peer.shutdown(3).await;
}

#[tokio::test]
async fn jetbrains_clients_with_a_usable_provider_get_no_auth_methods() {
    let mut peer = spawn_stdio_with_auth(
        EchoTurnDriver::new("ok"),
        None,
        Arc::new(StaticAuthEnvironment::dotenv_backed()),
    );
    peer.send(request(
        1,
        "initialize",
        json!({
            "protocolVersion": vibe_acp::ACP_PROTOCOL_VERSION,
            "clientInfo": {"name": "JetBrains.IntelliJ", "version": "2026.2"},
        }),
    ))
    .await;
    let (_, response) = peer.response(1).await;
    assert_eq!(response["result"]["authMethods"], json!([]));
    peer.shutdown(2).await;
}

#[tokio::test]
async fn the_auth_extension_methods_serve_status_and_sign_out() {
    let mut peer = spawn_stdio_with_auth(
        EchoTurnDriver::new("ok"),
        None,
        Arc::new(StaticAuthEnvironment::dotenv_backed()),
    );
    initialize(&mut peer, 1).await;
    peer.send(request(2, "_auth/status", json!({}))).await;
    let (_, status) = peer.response(2).await;
    assert_eq!(
        status["result"],
        json!({
            "authenticated": true,
            "authState": "vibe_home_env_file",
            "signOutAvailable": true,
            "customDomain": null,
        })
    );

    peer.send(request(3, "_auth/signOut", json!({}))).await;
    let (_, signed_out) = peer.response(3).await;
    assert_eq!(signed_out["result"], json!({}));

    peer.send(request(4, "_auth/status", json!({}))).await;
    let (_, status) = peer.response(4).await;
    assert_eq!(status["result"]["authState"], "signed_out");
    assert_eq!(status["result"]["signOutAvailable"], false);

    // Sign-out is now unavailable, so a second call is refused with the
    // invalid-request error and clears nothing.
    peer.send(request(5, "_auth/signOut", json!({}))).await;
    let (_, refused) = peer.response(5).await;
    assert_eq!(refused["error"]["code"], -32602);
    peer.shutdown(6).await;
}
