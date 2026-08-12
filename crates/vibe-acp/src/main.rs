#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinSet;
use vibe_acp::{
    AcpAgent, AcpAuthEnvironment, AcpClientFuture, AcpClientPort, AcpError, AcpForkSession,
    AcpInitializeRequest, AcpListSessions, AcpLoadSession, AcpNewSession, AcpSessionUpdate,
    ProductionAuthEnvironment, default_vibe_home,
};
use vibe_app_server::client::{
    CompactionDriverFuture, DriverError, DriverFuture, EventObserver, LiveDriverConfig,
    LiveTurnDriver, TurnDriver, TurnReservation,
};
use vibe_app_server::release3::{Release3Paths, Release3Service};
use vibe_app_server::transport::{MAX_FRAME_BYTES, read_bounded_frame};
use vibe_core::compaction::manager::CompactionPromptResolution;
use vibe_core::config::DotenvValues;
use vibe_core::observability::{LogLevel, init_file_logging, log};
use vibe_core::tracing::{TracingGuard, TracingSetup, setup_tracing};

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

    fn steer(
        &self,
        session_id: &str,
        turn_id: &str,
        content: &str,
        inject_invoked_skill: bool,
    ) -> Result<(), DriverError> {
        self.resolve()?
            .steer(session_id, turn_id, content, inject_invoked_skill)
    }

    fn inject_context(
        &self,
        session_id: &str,
        content: &str,
        as_message: bool,
        inject_invoked_skill: bool,
    ) -> Result<(), DriverError> {
        self.resolve()?
            .inject_context(session_id, content, as_message, inject_invoked_skill)
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
    let vibe_home = default_vibe_home();
    let session_root = vibe_home.join("sessions");
    // `{vibe_home}/.env` stands in for an unset process variable, which is what
    // the reference startup leaves behind after loading the file.
    let dotenv = DotenvValues::global(&vibe_home);
    // The log file opens before the first session, so an editor that never sees
    // stderr still leaves a trace on disk for the operator who reads one.
    install_file_logging(&vibe_home);
    let credential_environment = dotenv
        .variable("VIBE_CREDENTIAL_ENV")
        .unwrap_or_else(|| "MISTRAL_API_KEY".to_owned());
    // The span exporter is installed before the first session is opened, and
    // its guard lives as long as this process: dropping it flushes the batch.
    let _tracing = install_tracing(&vibe_home, &dotenv);
    let config = LiveDriverConfig {
        compaction_prompts: CompactionPromptResolution::default(),
        style: dotenv
            .variable("VIBE_PROVIDER_STYLE")
            .unwrap_or_else(|| "mistral".to_owned()),
        endpoint: dotenv
            .variable("VIBE_API_BASE")
            .unwrap_or_else(|| "https://api.mistral.ai/v1/chat/completions".to_owned()),
        model: dotenv
            .variable("VIBE_MODEL")
            .unwrap_or_else(|| "mistral-medium-3.5".to_owned()),
        credential_environment: credential_environment.clone(),
        system_prompt: "You are Mistral Vibe.".to_owned(),
        session_root: Some(session_root.clone()),
        input_price_per_million_micros: price_from_dotenv(&dotenv, "VIBE_INPUT_PRICE", 1_500_000)?,
        output_price_per_million_micros: price_from_dotenv(
            &dotenv,
            "VIBE_OUTPUT_PRICE",
            7_500_000,
        )?,
    };
    let driver = DeferredTurnDriver::new({
        let vibe_home = vibe_home.clone();
        move || LiveTurnDriver::from_environment(config.clone(), &DotenvValues::global(&vibe_home))
    });
    run_stdio(
        BufReader::new(tokio::io::stdin()),
        tokio::io::stdout(),
        driver,
        Some(session_root),
        credential_environment,
        Arc::new(ProductionAuthEnvironment::new(vibe_home)),
        true,
    )
    .await
}

/// Installs the span exporter this editor session exports through, when the
/// merged configuration asks for one. Reference reads `enable_telemetry` on the
/// ACP path too, and this port reads the same document the terminal client
/// reads.
fn install_tracing(vibe_home: &Path, dotenv: &DotenvValues) -> Option<TracingGuard> {
    let service = Release3Service::new(
        Release3Paths {
            vibe_home: vibe_home.to_path_buf(),
            working_directory: std::env::current_dir().unwrap_or_else(|_| vibe_home.to_path_buf()),
            session_root: vibe_home.join("sessions"),
        },
        false,
    )
    .ok()?;
    let snapshot = service.layered_config().load().ok()?;
    let credentials = |variable: &str| dotenv.variable(variable).filter(|value| !value.is_empty());
    match setup_tracing(&snapshot.effective, &credentials) {
        TracingSetup::Installed(guard) => Some(guard),
        TracingSetup::UnusableEndpoint { endpoint } => {
            report_degradation(&format!(
                "OTEL tracing is enabled but `{endpoint}` is not a usable collector; skipping."
            ));
            None
        }
        TracingSetup::MissingCredential { variable } => {
            report_degradation(&format!(
                "OTEL tracing is enabled but {variable} is not set; skipping."
            ));
            None
        }
        TracingSetup::Disabled => None,
    }
}

/// Reference `init_file_logging`, called from the ACP entrypoint before the
/// first session. A file that cannot be opened is reported once and the editor
/// session starts anyway.
fn install_file_logging(vibe_home: &Path) {
    let path = vibe_home.join("logs").join("vibe.log");
    if let Err(error) = init_file_logging(&path, &|name| std::env::var(name).ok()) {
        eprintln!("Logging to {} is unavailable: {error}", path.display());
    }
}

/// A startup degradation, told to the editor on stderr and left on disk for the
/// operator who reads the log afterward.
fn report_degradation(message: &str) {
    eprintln!("{message}");
    log(LogLevel::Warning, message);
}

async fn run_stdio<R, W, D>(
    mut reader: R,
    writer: W,
    driver: D,
    session_root: Option<PathBuf>,
    credential_environment: String,
    auth_environment: Arc<dyn AcpAuthEnvironment>,
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
    agent = agent
        .with_credential_environment(credential_environment)
        .with_auth_environment(auth_environment);
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
            let method_id = required_string(&request.params, "methodId")?.to_owned();
            agent.authenticate(&method_id, &request.params).await
        }
        // Extension methods reach the wire under a leading underscore, which
        // the reference's router strips before dispatching them.
        "_auth/status" => agent.auth_status(),
        "_auth/signOut" => agent.auth_sign_out(),
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
            agent
                .replay_history(&session.session_id, |update| send_update(writer, &update))
                .await?;
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
            let response = agent
                .prompt_content(&session_id, prompt, |update| send_update(writer, update))
                .await?;
            serde_json::to_value(response).map_err(AcpError::Json)
        }
        method => Err(AcpError::UnsupportedClientFlow(format!(
            "unknown ACP method `{method}`"
        ))),
    }
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
    error_response(id, error.json_rpc_code(), error.to_string())
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

fn price_from_dotenv(
    dotenv: &DotenvValues,
    name: &str,
    default_micros: u64,
) -> Result<u64, Box<dyn std::error::Error>> {
    let Some(value) = dotenv.variable(name) else {
        return Ok(default_micros);
    };
    let price = value.parse::<f64>()?;
    if !price.is_finite() || price < 0.0 || price > u64::MAX as f64 / 1_000_000.0 {
        return Err(format!("{name} must be a finite non-negative number").into());
    }
    Ok((price * 1_000_000.0).round() as u64)
}

#[cfg(test)]
mod stdio_tests;
