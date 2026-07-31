#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinSet;
use vibe_acp::{
    AcpAgent, AcpClientFuture, AcpClientPort, AcpError, AcpForkSession, AcpInitializeRequest,
    AcpListSessions, AcpLoadSession, AcpNewSession, AcpSessionUpdate, history_entry_updates,
};
use vibe_app_server::client::{
    CompactionDriverFuture, DriverError, DriverFuture, EventObserver, LiveDriverConfig,
    LiveTurnDriver, TurnDriver, TurnReservation,
};
use vibe_app_server::transport::{MAX_FRAME_BYTES, read_bounded_frame};

const WRITER_QUEUE_CAPACITY: usize = 1_024;
const MAX_CONCURRENT_REQUESTS: usize = 128;
const MAX_PENDING_CLIENT_REQUESTS: usize = 256;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireRequest {
    jsonrpc: String,
    #[serde(default)]
    id: Value,
    method: String,
    #[serde(default)]
    params: Value,
}

enum WriterMessage {
    Value(Value),
    Shutdown(oneshot::Sender<()>),
}

struct StdioClientPort {
    writer: mpsc::Sender<WriterMessage>,
    pending: Mutex<BTreeMap<i64, oneshot::Sender<Result<Value, String>>>>,
    next_id: AtomicI64,
}

impl StdioClientPort {
    fn new(writer: mpsc::Sender<WriterMessage>) -> Self {
        Self {
            writer,
            pending: Mutex::new(BTreeMap::new()),
            next_id: AtomicI64::new(1_000_000),
        }
    }

    fn resolve(&self, value: &Value) -> bool {
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

    fn disconnect(&self) {
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

struct DeferredTurnDriver<D> {
    driver: OnceLock<Arc<D>>,
    initialize: Mutex<()>,
    factory: Box<dyn Fn() -> Result<D, DriverError> + Send + Sync>,
}

impl<D> DeferredTurnDriver<D> {
    fn new(factory: impl Fn() -> Result<D, DriverError> + Send + Sync + 'static) -> Self {
        Self {
            driver: OnceLock::new(),
            initialize: Mutex::new(()),
            factory: Box::new(factory),
        }
    }

    fn resolve(&self) -> Result<&Arc<D>, DriverError> {
        if let Some(driver) = self.driver.get() {
            return Ok(driver);
        }
        let _guard = self
            .initialize
            .lock()
            .map_err(|_| DriverError::StatePoisoned)?;
        if self.driver.get().is_none() {
            let driver = Arc::new((self.factory)()?);
            self.driver
                .set(driver)
                .map_err(|_| DriverError::StatePoisoned)?;
        }
        self.driver.get().ok_or(DriverError::StatePoisoned)
    }
}

impl<D> TurnDriver for DeferredTurnDriver<D>
where
    D: TurnDriver + 'static,
{
    fn run<'a>(&'a self, reservation: &'a TurnReservation) -> DriverFuture<'a> {
        match self.resolve() {
            Ok(driver) => {
                let driver = driver.clone();
                Box::pin(async move { driver.run(reservation).await })
            }
            Err(error) => Box::pin(async move { Err(error) }),
        }
    }

    fn run_observed<'a>(
        &'a self,
        reservation: &'a TurnReservation,
        observer: Arc<dyn EventObserver>,
    ) -> DriverFuture<'a> {
        match self.resolve() {
            Ok(driver) => {
                let driver = driver.clone();
                Box::pin(async move { driver.run_observed(reservation, observer).await })
            }
            Err(error) => Box::pin(async move { Err(error) }),
        }
    }

    fn interrupt(&self, session_id: &str, turn_id: &str) -> Result<(), DriverError> {
        self.resolve()?.interrupt(session_id, turn_id)
    }

    fn steer(&self, session_id: &str, turn_id: &str, content: &str) -> Result<(), DriverError> {
        self.resolve()?.steer(session_id, turn_id, content)
    }

    fn inject_context(
        &self,
        session_id: &str,
        content: &str,
        as_message: bool,
    ) -> Result<(), DriverError> {
        self.resolve()?
            .inject_context(session_id, content, as_message)
    }

    fn resolve_callback(
        &self,
        session_id: &str,
        turn_id: &str,
        callback_id: &str,
        accepted: bool,
        value: Option<&str>,
    ) -> Result<(), DriverError> {
        self.resolve()?
            .resolve_callback(session_id, turn_id, callback_id, accepted, value)
    }

    fn compact<'a>(
        &'a self,
        session_id: &'a str,
        extra_instructions: &'a str,
    ) -> CompactionDriverFuture<'a> {
        match self.resolve() {
            Ok(driver) => {
                let driver = driver.clone();
                Box::pin(async move { driver.compact(session_id, extra_instructions).await })
            }
            Err(error) => Box::pin(async move { Err(error) }),
        }
    }
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
    let session_root = default_session_root();
    let credential_environment =
        std::env::var("VIBE_CREDENTIAL_ENV").unwrap_or_else(|_| "MISTRAL_API_KEY".to_owned());
    let config = LiveDriverConfig {
        style: std::env::var("VIBE_PROVIDER_STYLE").unwrap_or_else(|_| "mistral".to_owned()),
        endpoint: std::env::var("VIBE_API_BASE")
            .unwrap_or_else(|_| "https://api.mistral.ai/v1/chat/completions".to_owned()),
        model: std::env::var("VIBE_MODEL").unwrap_or_else(|_| "mistral-medium-3.5".to_owned()),
        credential_environment: credential_environment.clone(),
        system_prompt: "You are Mistral Vibe.".to_owned(),
        session_root: Some(session_root.clone()),
        input_price_per_million_micros: price_from_environment("VIBE_INPUT_PRICE", 1_500_000)?,
        output_price_per_million_micros: price_from_environment("VIBE_OUTPUT_PRICE", 7_500_000)?,
    };
    let driver = DeferredTurnDriver::new(move || LiveTurnDriver::from_environment(config.clone()));
    run_stdio(
        BufReader::new(tokio::io::stdin()),
        tokio::io::stdout(),
        driver,
        Some(session_root),
        credential_environment,
        true,
    )
    .await
}

async fn run_stdio<R, W, D>(
    mut reader: R,
    writer: W,
    driver: D,
    session_root: Option<PathBuf>,
    credential_environment: String,
    production_cloud: bool,
) -> Result<(), Box<dyn std::error::Error>>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
    D: TurnDriver + 'static,
{
    let (writer_tx, writer_rx) = mpsc::channel(WRITER_QUEUE_CAPACITY);
    let writer_task = tokio::spawn(writer_loop(writer, writer_rx));
    let client = Arc::new(StdioClientPort::new(writer_tx.clone()));
    let mut agent = AcpAgent::new(driver)?;
    if let Some(session_root) = session_root {
        agent = agent.with_session_root(session_root);
    }
    if production_cloud {
        agent = agent.with_production_cloud();
    }
    agent = agent.with_credential_environment(credential_environment);
    let agent =
        Arc::new(agent.with_client_port(client.clone(), vibe_acp::DEFAULT_CLIENT_TOOL_TIMEOUT));
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
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    writer_tx
        .send(WriterMessage::Shutdown(shutdown_tx))
        .await
        .map_err(|_| "ACP writer stopped before shutdown")?;
    let _ = shutdown_rx.await;
    writer_task.await??;
    Ok(())
}

fn default_session_root() -> PathBuf {
    std::env::var_os("VIBE_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .map(|home| home.join(".vibe"))
        })
        .unwrap_or_else(|| {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(".vibe")
        })
        .join("sessions")
}

async fn writer_loop<W>(
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

async fn handle_request<D>(
    agent: Arc<AcpAgent<D>>,
    request: WireRequest,
    writer: mpsc::Sender<WriterMessage>,
) where
    D: TurnDriver + 'static,
{
    let id = request.id.clone();
    let result = dispatch_request(agent, request, &writer).await;
    let response = match result {
        Ok(value) => success_response(id, value),
        Err(error) => acp_error_response(id, error),
    };
    let _ = send_value_wait(&writer, response).await;
}

async fn dispatch_request<D>(
    agent: Arc<AcpAgent<D>>,
    request: WireRequest,
    writer: &mpsc::Sender<WriterMessage>,
) -> Result<Value, AcpError>
where
    D: TurnDriver + 'static,
{
    if request.jsonrpc != "2.0" || !valid_id(&request.id) {
        return Err(AcpError::InvalidParams(
            "requests require jsonrpc 2.0 and a string or integer ID".to_owned(),
        ));
    }
    match request.method.as_str() {
        "initialize" => serde_json::from_value::<AcpInitializeRequest>(request.params)
            .map_err(AcpError::Json)
            .and_then(|params| agent.initialize_with(params))
            .and_then(|value| serde_json::to_value(value).map_err(AcpError::Json)),
        "authenticate" => {
            let method_id = required_string(&request.params, "methodId")?;
            agent.authenticate(method_id)?;
            Ok(json!({}))
        }
        "session/new" => {
            let params =
                serde_json::from_value::<AcpNewSession>(request.params).map_err(AcpError::Json)?;
            let session = agent.new_session(params)?;
            let encoded = serde_json::to_value(&session)?;
            send_commands_after_response(writer, &session.session_id, agent.advertised_commands());
            Ok(encoded)
        }
        "session/load" | "session/resume" => {
            let params =
                serde_json::from_value::<AcpLoadSession>(request.params).map_err(AcpError::Json)?;
            let session = agent.load_session(params)?;
            replay_history(&agent, &session.session_id, writer).await?;
            send_commands_after_response(writer, &session.session_id, agent.advertised_commands());
            let mut encoded = serde_json::to_value(session)?;
            if let Some(object) = encoded.as_object_mut() {
                object.remove("sessionId");
            }
            Ok(encoded)
        }
        "session/list" => {
            let params = serde_json::from_value::<AcpListSessions>(request.params)
                .map_err(AcpError::Json)?;
            serde_json::to_value(
                agent.list_sessions(params.cwd.as_deref(), params.cursor.as_deref())?,
            )
            .map_err(AcpError::Json)
        }
        "session/fork" => {
            let params =
                serde_json::from_value::<AcpForkSession>(request.params).map_err(AcpError::Json)?;
            let session = agent.fork_session(params)?;
            send_commands_after_response(writer, &session.session_id, agent.advertised_commands());
            serde_json::to_value(session).map_err(AcpError::Json)
        }
        "session/close" => {
            let session_id = required_string(&request.params, "sessionId")?;
            agent.close_session(session_id).await?;
            Ok(json!({}))
        }
        "session/set_mode" => {
            agent
                .set_mode(
                    required_string(&request.params, "sessionId")?,
                    required_string(&request.params, "modeId")?,
                )
                .await?;
            Ok(json!({}))
        }
        "session/set_config_option" => {
            let session_id = required_string(&request.params, "sessionId")?;
            let value = scalar_string(
                request
                    .params
                    .get("value")
                    .ok_or_else(|| AcpError::InvalidParams("value is required".to_owned()))?,
            )?;
            let config_options = agent
                .set_config_option(
                    session_id,
                    required_string(&request.params, "configId")?,
                    &value,
                )
                .await?;
            Ok(json!({"configOptions": config_options}))
        }
        "session/prompt" => {
            let session_id = required_string(&request.params, "sessionId")?.to_owned();
            let prompt = request
                .params
                .get("prompt")
                .and_then(Value::as_array)
                .cloned()
                .ok_or_else(|| {
                    AcpError::InvalidParams("session/prompt requires prompt blocks".to_owned())
                })?;
            let (sender, mut updates) = mpsc::channel(vibe_acp::MAX_ACP_UPDATE_QUEUE);
            let prompt_future = agent.prompt_content_streaming(&session_id, prompt, sender);
            tokio::pin!(prompt_future);
            let response = loop {
                tokio::select! {
                    response = &mut prompt_future => {
                        break response?;
                    }
                    update = updates.recv() => {
                        if let Some(update) = update
                            && let Err(error) = send_update(writer, &update) {
                                let _ = agent.cancel(&session_id).await;
                                return Err(error);
                            }
                    }
                }
            };
            while let Ok(update) = updates.try_recv() {
                if let Err(error) = send_update(writer, &update) {
                    let _ = agent.cancel(&session_id).await;
                    return Err(error);
                }
            }
            serde_json::to_value(response).map_err(AcpError::Json)
        }
        method => Err(AcpError::UnsupportedClientFlow(format!(
            "unknown ACP method `{method}`"
        ))),
    }
}

async fn replay_history<D>(
    agent: &AcpAgent<D>,
    session_id: &str,
    writer: &mpsc::Sender<WriterMessage>,
) -> Result<(), AcpError>
where
    D: TurnDriver,
{
    let mut offset = 0;
    loop {
        let page = agent.history(session_id, offset, 500).await?;
        for (index, entry) in page.entries.into_iter().enumerate() {
            for update in history_entry_updates(&entry, offset.saturating_add(index))? {
                send_update(
                    writer,
                    &AcpSessionUpdate {
                        session_id: session_id.to_owned(),
                        update,
                    },
                )?;
            }
        }
        let Some(next) = page.next_offset else {
            break;
        };
        if next <= offset {
            return Err(AcpError::InvalidResponse(
                "history cursor did not advance".to_owned(),
            ));
        }
        offset = next;
    }
    Ok(())
}

fn send_commands_after_response(
    writer: &mpsc::Sender<WriterMessage>,
    session_id: &str,
    commands: Vec<Value>,
) {
    let _ = send_value(
        writer,
        json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": session_id,
                "update": {
                    "sessionUpdate": "available_commands_update",
                    "availableCommands": commands,
                },
            },
        }),
    );
}

fn send_update(
    writer: &mpsc::Sender<WriterMessage>,
    update: &AcpSessionUpdate,
) -> Result<(), AcpError> {
    send_value(
        writer,
        json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": update,
        }),
    )
    .map_err(AcpError::ClientTool)
}

fn send_value(writer: &mpsc::Sender<WriterMessage>, value: Value) -> Result<(), String> {
    writer
        .try_send(WriterMessage::Value(value))
        .map_err(|error| format!("ACP writer queue is unavailable: {error}"))
}

async fn send_value_wait(writer: &mpsc::Sender<WriterMessage>, value: Value) -> Result<(), String> {
    writer
        .send(WriterMessage::Value(value))
        .await
        .map_err(|_| "ACP writer queue is closed".to_owned())
}

fn success_response(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn error_response(id: Value, code: i64, message: String) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {"code": code, "message": message},
    })
}

fn acp_error_response(id: Value, error: AcpError) -> Value {
    let code = match error {
        AcpError::UnsupportedClientFlow(_) => -32601,
        AcpError::InvalidParams(_)
        | AcpError::Json(_)
        | AcpError::UnsupportedProtocol(_)
        | AcpError::UnsupportedAuthentication(_) => -32602,
        AcpError::SessionNotFound(_) => -32001,
        AcpError::SessionConflict(_) | AcpError::AlreadyInitialized => -32002,
        AcpError::NotInitialized => -32003,
        _ => -32603,
    };
    error_response(id, code, error.to_string())
}

fn valid_id(id: &Value) -> bool {
    id.is_i64() || id.is_u64() || id.is_string()
}

fn required_string<'a>(params: &'a Value, key: &str) -> Result<&'a str, AcpError> {
    params
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| AcpError::InvalidParams(format!("{key} must be a non-empty string")))
}

fn scalar_string(value: &Value) -> Result<String, AcpError> {
    match value {
        Value::String(value) => Ok(value.clone()),
        Value::Bool(value) => Ok(value.to_string()),
        Value::Number(value) => Ok(value.to_string()),
        _ => Err(AcpError::InvalidParams(
            "config value must be a string, boolean, or number".to_owned(),
        )),
    }
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

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;

    use tokio::io::{
        AsyncBufReadExt, AsyncWriteExt, DuplexStream, ReadHalf, WriteHalf, duplex, split,
    };
    use tokio::task::JoinHandle;
    use vibe_app_server::client::{EchoTurnDriver, TurnReservation};

    use super::*;

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
            let bytes =
                tokio::time::timeout(Duration::from_secs(2), self.reader.read_line(&mut line))
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

    async fn prompt(
        peer: &mut TestPeer,
        id: i64,
        session_id: &str,
        text: &str,
    ) -> (Vec<Value>, Value) {
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
        let error = send_value(&writer, json!({"second": true}))
            .expect_err("second value exceeds the bound");
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
                LiveTurnDriver::from_environment(LiveDriverConfig {
                    style: "mistral".to_owned(),
                    endpoint: "http://127.0.0.1:1".to_owned(),
                    model: "test-model".to_owned(),
                    credential_environment: MISSING_CREDENTIAL.to_owned(),
                    system_prompt: "test".to_owned(),
                    session_root: None,
                    input_price_per_million_micros: 0,
                    output_price_per_million_micros: 0,
                })
            }
        });
        let mut peer = spawn_stdio(driver);

        let initialized = initialize(&mut peer, 1).await;
        assert_eq!(initialized["result"]["authMethods"][0]["id"], "environment");
        assert_eq!(
            initialized["result"]["authMethods"][0]["vars"][0]["name"],
            "MISTRAL_API_KEY"
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
            update["sessionUpdate"] != "agent_message_chunk"
                || update["content"]["text"].is_string()
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
                || message.pointer("/params/sessionId").and_then(Value::as_str)
                    == Some(first.as_str())
        }));

        let (second_updates, second_response) = prompt(&mut peer, 5, &second, "second").await;
        assert!(second_response.get("error").is_none());
        assert!(second_updates.iter().all(|message| {
            message.get("method").and_then(Value::as_str) != Some("session/update")
                || message.pointer("/params/sessionId").and_then(Value::as_str)
                    == Some(second.as_str())
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
}
