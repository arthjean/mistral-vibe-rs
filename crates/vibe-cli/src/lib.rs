#![cfg_attr(
    test,
    allow(
        clippy::expect_used,
        clippy::panic,
        clippy::type_complexity,
        clippy::unwrap_in_result,
        clippy::unwrap_used
    )
)]

pub mod distribution;
pub mod tui;

use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use clap::{ArgAction, Parser, ValueEnum};
use secrecy::SecretString;
use serde::Serialize;
use thiserror::Error;
use url::Url;
use vibe_app_server::client::{
    ClientError, HeadlessService, LiveDriverConfig, LiveTurnDriver, ProgrammaticTeleportEvent,
    ProgrammaticTurn, ProgrammaticUpdate, PublicTurnStopReason, SessionOptions, TurnDriver,
    programmatic_update_channel,
};
use vibe_app_server::release3::Release3Service;
use vibe_app_server::release4::{Release4Service, VibeCodeCloudConfig};
use vibe_app_server::resources::{
    CoreResourceBackend, MistralConnectorClient, production_mcp_adapters,
};
use vibe_app_server::server::AppServer;
use vibe_core::telemetry::{
    ReqwestTelemetryTransport, TelemetryAttributes, TelemetryClient, TelemetryConfig,
    TelemetryEnvelope, TelemetryEvent, TelemetryEventObserver, TelemetryField, TelemetryMetadata,
};
use vibe_core::{engine::EventObserver, events::EventEnvelope};

pub const ADAPTER_NAME: &str = "vibe-cli";

#[derive(Debug, Clone, Parser)]
#[command(
    name = "vibe",
    version,
    disable_version_flag = true,
    about = "Run the Mistral Vibe interactive CLI"
)]
pub struct Arguments {
    pub initial_prompt: Option<String>,
    #[arg(short = 'v', long = "version", action = ArgAction::Version)]
    pub version: Option<bool>,
    #[arg(short = 'p', long, num_args = 0..=1, default_missing_value = "")]
    pub prompt: Option<String>,
    #[arg(long, value_enum, default_value_t = OutputMode::Text)]
    pub output: OutputMode,
    #[arg(
        long,
        conflicts_with = "continue_session",
        num_args = 0..=1,
        default_missing_value = ""
    )]
    pub resume: Option<String>,
    #[arg(short = 'c', long = "continue", conflicts_with = "resume")]
    pub continue_session: bool,
    #[arg(long)]
    pub workdir: Option<PathBuf>,
    #[arg(long = "add-dir")]
    pub add_directories: Vec<PathBuf>,
    #[arg(long)]
    pub trust: bool,
    #[arg(long)]
    pub agent: Option<String>,
    #[arg(long = "enabled-tools", action = ArgAction::Append)]
    pub enabled_tools: Vec<String>,
    #[arg(long = "disabled-tools", action = ArgAction::Append)]
    pub disabled_tools: Vec<String>,
    #[arg(long = "allowed-tool", hide = true)]
    pub tool_filters: Vec<String>,
    #[arg(long)]
    pub max_turns: Option<u32>,
    #[arg(long)]
    pub max_tokens: Option<u64>,
    #[arg(long)]
    pub max_price: Option<f64>,
    #[arg(long, visible_alias = "yolo")]
    pub auto_approve: bool,
    #[arg(long)]
    pub setup: bool,
    #[arg(long, action = ArgAction::SetTrue)]
    pub check_upgrade: bool,
    #[arg(long, action = ArgAction::SetTrue)]
    pub telemetry: bool,
    #[arg(long)]
    pub worktree: Option<String>,
    #[arg(long, hide = true)]
    pub teleport: bool,
    #[arg(long, default_value = "mistral", hide = true)]
    pub provider_style: String,
    #[arg(long, default_value = "mistral-medium-3.5", hide = true)]
    pub model: String,
    #[arg(long, default_value_t = 1.5, hide = true)]
    pub input_price: f64,
    #[arg(long, default_value_t = 7.5, hide = true)]
    pub output_price: f64,
    #[arg(
        long,
        default_value = "https://api.mistral.ai/v1/chat/completions",
        hide = true
    )]
    pub api_base: String,
    #[arg(long, default_value = "MISTRAL_API_KEY", hide = true)]
    pub credential_environment: String,
    #[arg(long, hide = true)]
    pub session_root: Option<PathBuf>,
    #[arg(long, hide = true)]
    pub fake_response: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum OutputMode {
    #[default]
    Text,
    Json,
    #[value(alias = "ndjson")]
    Streaming,
}

pub async fn run(
    arguments: Arguments,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> Result<(), CliError> {
    if let Some(response) = &arguments.fake_response {
        execute(
            arguments.clone(),
            vibe_app_server::client::EchoTurnDriver::new(response),
            stdout,
            stderr,
        )
        .await
    } else {
        let config = LiveDriverConfig {
            style: arguments.provider_style.clone(),
            endpoint: arguments.api_base.clone(),
            model: arguments.model.clone(),
            credential_environment: arguments.credential_environment.clone(),
            system_prompt: "You are Mistral Vibe.".to_owned(),
            session_root: arguments.session_root.clone(),
            input_price_per_million_micros: price_per_million_micros(arguments.input_price)?,
            output_price_per_million_micros: price_per_million_micros(arguments.output_price)?,
        };
        let credential = std::env::var(&arguments.credential_environment).map_err(|_| {
            vibe_app_server::client::DriverError::MissingCredentialEnvironment(
                arguments.credential_environment.clone(),
            )
        })?;
        let telemetry = telemetry_event_observer(&arguments, &credential, "cli")?;
        let mut driver = LiveTurnDriver::from_credential(config, credential)?;
        if let Some(observer) = telemetry.as_ref() {
            driver = driver.with_event_observer(observer.clone());
        }
        let server = production_server(&arguments)?;
        let result = execute_with_server(arguments, driver, server, stdout, stderr).await;
        if let Some(observer) = telemetry {
            observer.flush().await;
        }
        result
    }
}

pub async fn execute<D>(
    arguments: Arguments,
    driver: D,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> Result<(), CliError>
where
    D: TurnDriver,
{
    execute_with_server(arguments, driver, AppServer::default(), stdout, stderr).await
}

async fn execute_with_server<D>(
    arguments: Arguments,
    driver: D,
    server: AppServer,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> Result<(), CliError>
where
    D: TurnDriver,
{
    validate_arguments(&arguments)?;
    let prompt = arguments
        .prompt
        .clone()
        .or_else(|| arguments.initial_prompt.clone())
        .ok_or_else(|| {
            CliError::InvalidArguments(
                "programmatic mode requires --prompt or an initial prompt".to_owned(),
            )
        })?;
    let working_directory = match arguments.workdir {
        Some(path) => path,
        None => std::env::current_dir().map_err(CliError::CurrentDirectory)?,
    };
    let max_price_micros = arguments
        .max_price
        .map(|price| (price * 1_000_000.0).round() as u64);
    let session_id = arguments.resume.clone();
    let options = SessionOptions {
        working_directory: working_directory.to_string_lossy().into_owned(),
        session_id,
        add_directories: arguments
            .add_directories
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect(),
        trusted: arguments.trust,
        agent: arguments.agent.clone(),
        tool_filters: arguments.tool_filters.clone(),
        enabled_tools: arguments.enabled_tools.clone(),
        disabled_tools: arguments.disabled_tools.clone(),
        mcp_servers: Vec::new(),
        model: Some(arguments.model.clone()),
        max_turns: arguments.max_turns,
        max_tokens: arguments.max_tokens,
        max_price_micros,
        mode: None,
        thinking: false,
        reasoning_effort: None,
        auto_approve: arguments.auto_approve,
        resume: arguments.resume.clone(),
        continue_session: arguments.continue_session,
    };
    let mut service = HeadlessService::new_shared_with_server(Arc::new(driver), server)?;
    let session_id = service.start_session(&options)?;
    let mut close_session_id = session_id.clone();
    let turn_result: Result<ProgrammaticTurn, CliError> = if arguments.teleport {
        service
            .teleport(
                &session_id,
                &working_directory.to_string_lossy(),
                &prompt,
                true,
            )
            .await
            .map_err(|error| CliError::Teleport(error.to_string()))
            .and_then(|teleport_events| {
                let history = service
                    .session(&session_id)
                    .map_err(CliError::Client)?
                    .snapshot
                    .map(|snapshot| snapshot.history)
                    .unwrap_or_default();
                Ok(ProgrammaticTurn {
                    session_id: session_id.clone(),
                    turn_id: String::new(),
                    final_assistant: String::new(),
                    history,
                    events: Vec::new(),
                    usage: vibe_app_server::client::PublicUsage {
                        input_tokens: 0,
                        output_tokens: 0,
                        price_micros: 0,
                    },
                    context_tokens: 0,
                    steps: 0,
                    checkpoints: 0,
                    stop_reason: PublicTurnStopReason::Complete,
                    teleport_events,
                })
            })
    } else if arguments.output == OutputMode::Streaming {
        let (observer, mut updates) = programmatic_update_channel(&session_id);
        let prompt_future = service.prompt_observed(&session_id, &prompt, observer);
        tokio::pin!(prompt_future);
        let mut result = loop {
            tokio::select! {
                result = &mut prompt_future => break result.map_err(CliError::Client),
                update = updates.recv() => {
                    let Some(ProgrammaticUpdate::HistoryEntry { entry, .. }) = update else {
                        continue;
                    };
                    if let Err(error) = write_json_line(stdout, &entry)
                        .and_then(|()| stdout.flush().map_err(CliError::Stdout))
                    {
                        break Err(error);
                    }
                }
            }
        };
        if result.is_ok() {
            // Every queued update is consumed: stopping at the first non-entry
            // update would truncate the stream the turn already produced.
            while let Ok(update) = updates.try_recv() {
                let ProgrammaticUpdate::HistoryEntry { entry, .. } = update else {
                    continue;
                };
                if let Err(error) = write_json_line(stdout, &entry) {
                    result = Err(error);
                    break;
                }
            }
        }
        result
    } else {
        service
            .prompt(&session_id, &prompt)
            .await
            .map_err(CliError::Client)
    };
    let execution = match turn_result {
        Ok(turn) => {
            close_session_id.clone_from(&turn.session_id);
            let teleport_url =
                write_teleport_events(arguments.output, &turn.teleport_events, stdout);
            match (turn.stop_reason.clone(), teleport_url) {
                (_, Err(error)) => Err(error),
                (PublicTurnStopReason::Complete, Ok(teleport_url)) => {
                    if arguments.output == OutputMode::Streaming {
                        Ok(())
                    } else {
                        write_turn(
                            arguments.output,
                            &turn,
                            teleport_url.as_deref(),
                            stdout,
                            stderr,
                        )
                    }
                }
                (
                    PublicTurnStopReason::MaxSteps
                    | PublicTurnStopReason::TokenLimit
                    | PublicTurnStopReason::PriceLimit,
                    Ok(_),
                ) => Err(CliError::Limit(if !turn.final_assistant.is_empty() {
                    turn.final_assistant
                } else {
                    "The configured conversation limit was reached".to_owned()
                })),
                (PublicTurnStopReason::ResponseLength, Ok(_)) => Err(CliError::TurnFailed(
                    "The model's response exceeded the maximum output token limit.".to_owned(),
                )),
                (PublicTurnStopReason::Refusal, Ok(_)) => Err(CliError::TurnFailed(
                    "Provider refused the request".to_owned(),
                )),
                (PublicTurnStopReason::Cancelled, Ok(_)) => {
                    Err(CliError::TurnFailed("Turn cancelled".to_owned()))
                }
                (PublicTurnStopReason::Failed, Ok(_)) => {
                    Err(CliError::TurnFailed("Turn failed".to_owned()))
                }
            }
        }
        Err(error) => Err(error),
    };
    let close_result = service.close_session(&close_session_id).await;
    let shutdown_result = service.shutdown();
    execution?;
    close_result?;
    shutdown_result?;
    Ok(())
}

fn production_server(arguments: &Arguments) -> Result<AppServer, CliError> {
    let release3 = Release3Service::default();
    let credential = std::env::var(&arguments.credential_environment).map_err(|_| {
        vibe_app_server::client::DriverError::MissingCredentialEnvironment(
            arguments.credential_environment.clone(),
        )
    })?;
    let connector = Arc::new(
        MistralConnectorClient::new(&arguments.api_base, credential.clone())
            .map_err(|error| CliError::Terminal(error.to_string()))?,
    );
    let (mcp_factory, mcp_auth) =
        production_mcp_adapters().map_err(|error| CliError::Terminal(error.to_string()))?;
    let resource_backend = CoreResourceBackend::default()
        .with_config(release3.layered_config())
        .with_mcp_factory(mcp_factory)
        .with_mcp_auth(mcp_auth)
        .with_connector_catalog(
            connector.clone(),
            connector.clone(),
            arguments.credential_environment.clone(),
            connector.base_url(),
        )
        .with_connector_auth(connector);
    let server = AppServer::with_resource_backend(Arc::new(resource_backend))
        .using_release3_service(release3);
    if !arguments.teleport {
        return Ok(server);
    }
    let config = VibeCodeCloudConfig::from_credential(credential)
        .map_err(|error| CliError::Teleport(error.to_string()))?;
    let release4 = Release4Service::production(config)
        .map_err(|error| CliError::Teleport(error.to_string()))?;
    Ok(server.using_release4_service(release4))
}

fn validate_arguments(arguments: &Arguments) -> Result<(), CliError> {
    if arguments
        .prompt
        .as_deref()
        .is_some_and(|prompt| prompt.trim().is_empty())
    {
        return Err(CliError::InvalidArguments(
            "No prompt provided for programmatic mode".to_owned(),
        ));
    }
    if arguments.resume.as_deref() == Some("") && arguments.prompt.is_some() {
        return Err(CliError::InvalidArguments(
            "--resume requires a session ID in programmatic mode".to_owned(),
        ));
    }
    if arguments
        .max_price
        .is_some_and(|price| !price.is_finite() || price < 0.0)
    {
        return Err(CliError::InvalidArguments(
            "max-price must be a finite non-negative number".to_owned(),
        ));
    }
    for (name, price) in [
        ("input-price", arguments.input_price),
        ("output-price", arguments.output_price),
    ] {
        if !price.is_finite() || price < 0.0 {
            return Err(CliError::InvalidArguments(format!(
                "{name} must be a finite non-negative number"
            )));
        }
    }
    Ok(())
}

fn price_per_million_micros(price: f64) -> Result<u64, CliError> {
    if !price.is_finite() || price < 0.0 || price > u64::MAX as f64 / 1_000_000.0 {
        return Err(CliError::InvalidArguments(
            "model pricing must be a finite non-negative number".to_owned(),
        ));
    }
    Ok((price * 1_000_000.0).round() as u64)
}

fn write_turn(
    mode: OutputMode,
    turn: &ProgrammaticTurn,
    teleport_url: Option<&str>,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> Result<(), CliError> {
    match mode {
        OutputMode::Text => {
            writeln!(
                stdout,
                "{}",
                teleport_url.unwrap_or(turn.final_assistant.as_str())
            )
            .map_err(CliError::Stdout)?;
        }
        OutputMode::Json => {
            if let Some(url) = teleport_url {
                serde_json::to_writer_pretty(
                    &mut *stdout,
                    &serde_json::json!({
                        "history": &turn.history,
                        "teleportUrl": url,
                    }),
                )
                .map_err(CliError::Json)?;
            } else {
                serde_json::to_writer_pretty(&mut *stdout, &turn.history)
                    .map_err(CliError::Json)?;
            }
            stdout.write_all(b"\n").map_err(CliError::Stdout)?;
        }
        OutputMode::Streaming => {}
    }
    stdout.flush().map_err(CliError::Stdout)?;
    stderr.flush().map_err(CliError::Stderr)
}

fn write_teleport_events(
    mode: OutputMode,
    events: &[ProgrammaticTeleportEvent],
    stdout: &mut impl Write,
) -> Result<Option<String>, CliError> {
    let mut completed_url = None;
    for event in events {
        if mode == OutputMode::Streaming {
            write_json_line(stdout, event)?;
            stdout.flush().map_err(CliError::Stdout)?;
        } else if mode == OutputMode::Text {
            let progress = match event {
                ProgrammaticTeleportEvent::SummarizingContext { .. } => {
                    Some("Summarizing context...")
                }
                ProgrammaticTeleportEvent::CheckingGit { .. } => Some("Preparing workspace..."),
                ProgrammaticTeleportEvent::PushRequired { unpushed_count, .. } => {
                    writeln!(stdout, "Pushing {unpushed_count} commit(s)...")
                        .map_err(CliError::Stdout)?;
                    None
                }
                ProgrammaticTeleportEvent::Pushing { .. } => Some("Syncing with remote..."),
                ProgrammaticTeleportEvent::StartingWorkflow { .. } => Some("Teleporting..."),
                ProgrammaticTeleportEvent::Complete { .. }
                | ProgrammaticTeleportEvent::Failed { .. } => None,
            };
            if let Some(progress) = progress {
                writeln!(stdout, "{progress}").map_err(CliError::Stdout)?;
            }
        }
        match event {
            ProgrammaticTeleportEvent::Complete { url, .. } => {
                completed_url = Some(url.clone());
            }
            ProgrammaticTeleportEvent::Failed { error, .. } => {
                return Err(CliError::Teleport(error.message.clone()));
            }
            _ => {}
        }
    }
    Ok(completed_url)
}

fn write_json_line(writer: &mut impl Write, value: &impl Serialize) -> Result<(), CliError> {
    serde_json::to_writer(&mut *writer, value).map_err(CliError::Json)?;
    writer.write_all(b"\n").map_err(CliError::Stdout)
}

#[derive(Debug, Error)]
pub enum CliError {
    #[error("invalid arguments: {0}")]
    InvalidArguments(String),
    #[error("cannot resolve current directory: {0}")]
    CurrentDirectory(std::io::Error),
    #[error("stdout write failed: {0}")]
    Stdout(std::io::Error),
    #[error("stderr write failed: {0}")]
    Stderr(std::io::Error),
    #[error("{0}")]
    Limit(String),
    #[error("{0}")]
    TurnFailed(String),
    #[error("{0}")]
    Teleport(String),
    #[error("terminal UI failed: {0}")]
    Terminal(String),
    #[error(transparent)]
    Startup(#[from] tui::startup::StartupError),
    #[error("telemetry setup failed: {0}")]
    Telemetry(String),
    #[error(transparent)]
    Json(serde_json::Error),
    #[error(transparent)]
    Client(#[from] ClientError),
    #[error(transparent)]
    Driver(#[from] vibe_app_server::client::DriverError),
}

#[cfg(test)]
pub(crate) fn arguments_for_test() -> Arguments {
    Arguments {
        initial_prompt: None,
        version: None,
        prompt: None,
        output: OutputMode::Text,
        resume: None,
        continue_session: false,
        workdir: None,
        add_directories: Vec::new(),
        trust: false,
        agent: None,
        enabled_tools: Vec::new(),
        disabled_tools: Vec::new(),
        tool_filters: Vec::new(),
        max_turns: None,
        max_tokens: None,
        max_price: None,
        auto_approve: false,
        setup: false,
        check_upgrade: false,
        telemetry: false,
        worktree: None,
        teleport: false,
        provider_style: "mistral".to_owned(),
        model: "mistral-medium-3.5".to_owned(),
        input_price: 1.5,
        output_price: 7.5,
        api_base: "https://api.mistral.ai/v1/chat/completions".to_owned(),
        credential_environment: "MISTRAL_API_KEY".to_owned(),
        session_root: None,
        fake_response: None,
    }
}

pub(crate) struct CliTelemetryObserver {
    events: TelemetryEventObserver<ReqwestTelemetryTransport>,
    feedback: Arc<TelemetryClient<ReqwestTelemetryTransport>>,
    metadata: TelemetryMetadata,
    pending_feedback: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl CliTelemetryObserver {
    fn new(
        event_config: TelemetryConfig,
        feedback_config: TelemetryConfig,
        transport: ReqwestTelemetryTransport,
        metadata: TelemetryMetadata,
    ) -> Self {
        let feedback = Arc::new(TelemetryClient::new(feedback_config, transport.clone()));
        Self {
            events: TelemetryEventObserver::new(
                TelemetryClient::new(event_config, transport),
                metadata.clone(),
            ),
            feedback,
            metadata,
            pending_feedback: Mutex::new(Vec::new()),
        }
    }

    /// Queues best-effort telemetry after validating the complete envelope.
    /// Delivery errors follow the same intentionally silent policy as engine
    /// telemetry and all queued work is joined by [`Self::flush`].
    pub(crate) fn enqueue_feedback(&self, rating: u8, model: &str) -> Result<(), String> {
        let envelope = feedback_envelope(rating, model, self.metadata.clone())?;
        let client = Arc::clone(&self.feedback);
        let task = tokio::spawn(async move {
            let _ = client.record(&envelope).await;
        });
        if let Ok(mut pending) = self.pending_feedback.lock() {
            pending.retain(|task| !task.is_finished());
            pending.push(task);
        }
        Ok(())
    }

    pub(crate) async fn flush(&self) {
        self.events.flush().await;
        let pending = self
            .pending_feedback
            .lock()
            .map(|mut pending| std::mem::take(&mut *pending))
            .unwrap_or_default();
        for task in pending {
            let _ = task.await;
        }
    }
}

impl EventObserver for CliTelemetryObserver {
    fn observe(&self, event: &EventEnvelope) -> Result<(), String> {
        self.events.observe(event)
    }
}

fn feedback_envelope(
    rating: u8,
    model: &str,
    metadata: TelemetryMetadata,
) -> Result<TelemetryEnvelope, String> {
    let mut attributes = TelemetryAttributes::default();
    attributes
        .count(TelemetryField::Rating, u64::from(rating))
        .label(TelemetryField::Version, env!("CARGO_PKG_VERSION"))
        .map_err(|error| error.to_string())?
        .label(TelemetryField::Model, model)
        .map_err(|error| error.to_string())?;
    TelemetryEnvelope::new(
        TelemetryEvent::FeedbackSubmitted,
        metadata,
        attributes,
        None,
    )
    .map_err(|error| error.to_string())
}

pub(crate) fn telemetry_event_observer(
    arguments: &Arguments,
    credential: &str,
    entrypoint: &str,
) -> Result<Option<Arc<CliTelemetryObserver>>, CliError> {
    if !arguments.telemetry || arguments.provider_style != "mistral" || credential.is_empty() {
        return Ok(None);
    }
    let api_base =
        Url::parse(&arguments.api_base).map_err(|error| CliError::Telemetry(error.to_string()))?;
    let credential = SecretString::from(credential.to_owned());
    let event_config = TelemetryConfig::new(true, &api_base, Some(credential.clone()))
        .map_err(|error| CliError::Telemetry(error.to_string()))?;
    let feedback_config = TelemetryConfig::new(true, &api_base, Some(credential))
        .map_err(|error| CliError::Telemetry(error.to_string()))?;
    let transport = ReqwestTelemetryTransport::try_new()
        .map_err(|error| CliError::Telemetry(error.to_string()))?;
    let metadata = TelemetryMetadata::new(
        env!("CARGO_PKG_VERSION"),
        format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
        entrypoint,
    )
    .map_err(|error| CliError::Telemetry(error.to_string()))?;
    Ok(Some(Arc::new(CliTelemetryObserver::new(
        event_config,
        feedback_config,
        transport,
        metadata,
    ))))
}

impl CliError {
    #[must_use]
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::InvalidArguments(_) => 2,
            Self::Driver(vibe_app_server::client::DriverError::MissingCredentialEnvironment(_)) => {
                4
            }
            _ => 1,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io;

    use super::*;
    use vibe_app_server::client::{DriverFuture, EchoTurnDriver, TurnReservation};

    struct NeverTurnDriver;

    impl TurnDriver for NeverTurnDriver {
        fn run<'a>(&'a self, _reservation: &'a TurnReservation) -> DriverFuture<'a> {
            panic!("Teleport must not run a model turn")
        }
    }

    struct BrokenStdout;

    impl Write for BrokenStdout {
        fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed reader"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed reader"))
        }
    }

    fn arguments(mode: OutputMode) -> Arguments {
        Arguments {
            initial_prompt: None,
            version: None,
            prompt: Some("hello".to_owned()),
            output: mode,
            resume: None,
            continue_session: false,
            workdir: Some(PathBuf::from("/workspace")),
            add_directories: vec![PathBuf::from("/shared")],
            trust: true,
            agent: Some("coder".to_owned()),
            enabled_tools: vec!["read".to_owned()],
            disabled_tools: vec!["shell".to_owned()],
            tool_filters: vec!["read".to_owned()],
            max_turns: Some(4),
            max_tokens: Some(1000),
            max_price: Some(0.01),
            auto_approve: true,
            setup: false,
            check_upgrade: true,
            telemetry: false,
            worktree: None,
            teleport: false,
            provider_style: "mistral".to_owned(),
            model: "test".to_owned(),
            input_price: 1.5,
            output_price: 7.5,
            api_base: "https://provider.invalid".to_owned(),
            credential_environment: "TEST_API_KEY".to_owned(),
            session_root: None,
            fake_response: Some("world".to_owned()),
        }
    }

    #[test]
    fn incompatible_session_intents_are_rejected_by_clap() {
        let parsed = Arguments::try_parse_from([
            "vibe",
            "--prompt",
            "hello",
            "--resume",
            "session-1",
            "--continue",
        ]);
        assert!(parsed.is_err());
    }

    #[test]
    fn interactive_mode_does_not_require_the_version_flag() {
        let parsed = Arguments::try_parse_from(["vibe"]).expect("interactive arguments");
        assert!(parsed.version.is_none());
        assert!(parsed.initial_prompt.is_none());
        assert!(parsed.prompt.is_none());
    }

    #[test]
    fn empty_programmatic_prompt_is_rejected() {
        let mut arguments = arguments(OutputMode::Text);
        arguments.prompt = Some("  ".to_owned());
        assert!(matches!(
            validate_arguments(&arguments),
            Err(CliError::InvalidArguments(message))
                if message == "No prompt provided for programmatic mode"
        ));
    }

    #[test]
    fn telemetry_is_explicitly_opt_in() {
        let disabled = arguments(OutputMode::Text);
        assert!(
            telemetry_event_observer(&disabled, "credential", "cli")
                .expect("disabled telemetry")
                .is_none()
        );
        let mut enabled = disabled;
        enabled.telemetry = true;
        assert!(
            telemetry_event_observer(&enabled, "credential", "cli")
                .expect("enabled telemetry")
                .is_some()
        );
    }

    #[test]
    fn feedback_telemetry_keeps_the_numeric_rating_version_and_model() {
        let metadata =
            TelemetryMetadata::new("2.23.1", "linux-x86_64", "tui").expect("safe metadata");
        let envelope =
            feedback_envelope(3, "mistral-medium-3.5", metadata).expect("feedback envelope");
        let value = serde_json::to_value(envelope).expect("telemetry JSON");

        assert_eq!(value["event"], "vibe.user_rating_feedback");
        assert_eq!(value["properties"]["attributes"]["rating"], 3);
        assert_eq!(
            value["properties"]["attributes"]["version"],
            env!("CARGO_PKG_VERSION")
        );
        assert_eq!(
            value["properties"]["attributes"]["model"],
            "mistral-medium-3.5"
        );
    }

    #[tokio::test]
    async fn text_json_and_streaming_have_deterministic_channels() {
        for (mode, expected) in [
            (OutputMode::Text, "world\n"),
            (OutputMode::Json, "\"role\": \"assistant\""),
            (OutputMode::Streaming, "\"role\":\"assistant\""),
        ] {
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            execute(
                arguments(mode),
                EchoTurnDriver::new("world"),
                &mut stdout,
                &mut stderr,
            )
            .await
            .expect("programmatic run");
            let stdout = String::from_utf8(stdout).expect("UTF-8 output");
            assert!(stdout.contains(expected), "{stdout}");
            assert!(stderr.is_empty());
        }
    }

    #[tokio::test]
    async fn broken_stdout_is_typed_after_session_cleanup_is_attempted() {
        let mut stdout = BrokenStdout;
        let mut stderr = Vec::new();
        let error = execute(
            arguments(OutputMode::Text),
            EchoTurnDriver::new("world"),
            &mut stdout,
            &mut stderr,
        )
        .await
        .expect_err("broken stdout fails");
        assert!(matches!(error, CliError::Stdout(_)));
        assert_eq!(error.exit_code(), 1);
    }

    #[tokio::test]
    async fn teleport_flag_bypasses_the_model_and_reports_unavailable_git_context() {
        let mut arguments = arguments(OutputMode::Text);
        arguments.teleport = true;
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let error = execute(arguments, NeverTurnDriver, &mut stdout, &mut stderr)
            .await
            .expect_err("default cloud backend is unavailable");

        assert!(
            matches!(
                error,
                CliError::Teleport(ref message)
                    if message.contains("not an inspectable Git repository")
            ),
            "{error:?}"
        );
        assert!(stdout.is_empty());
    }

    #[test]
    fn missing_provider_credentials_have_the_compatible_exit_code() {
        let error = CliError::Driver(
            vibe_app_server::client::DriverError::MissingCredentialEnvironment(
                "MISSING_API_KEY".to_owned(),
            ),
        );
        assert_eq!(error.exit_code(), 4);
    }

    #[test]
    fn teleport_events_preserve_progress_and_json_result_url() {
        let events = vec![
            ProgrammaticTeleportEvent::CheckingGit {
                operation_id: "teleport-1".to_owned(),
            },
            ProgrammaticTeleportEvent::Complete {
                operation_id: "teleport-1".to_owned(),
                url: "https://vibe.example/run".to_owned(),
            },
        ];
        let mut progress = Vec::new();
        let url = write_teleport_events(OutputMode::Text, &events, &mut progress)
            .expect("teleport events");
        assert_eq!(
            String::from_utf8(progress).expect("UTF-8 progress"),
            "Preparing workspace...\n"
        );

        let turn = ProgrammaticTurn {
            session_id: "session-1".to_owned(),
            turn_id: "turn-1".to_owned(),
            final_assistant: String::new(),
            history: Vec::new(),
            events: Vec::new(),
            usage: vibe_app_server::client::PublicUsage {
                input_tokens: 0,
                output_tokens: 0,
                price_micros: 0,
            },
            context_tokens: 0,
            steps: 0,
            checkpoints: 0,
            stop_reason: PublicTurnStopReason::Complete,
            teleport_events: events,
        };
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        write_turn(
            OutputMode::Json,
            &turn,
            url.as_deref(),
            &mut stdout,
            &mut stderr,
        )
        .expect("JSON output");
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&stdout).expect("JSON"),
            serde_json::json!({
                "history": [],
                "teleportUrl": "https://vibe.example/run",
            })
        );
    }

    #[test]
    fn invalid_limits_are_typed_before_runtime_creation() {
        let mut arguments = arguments(OutputMode::Text);
        arguments.max_price = Some(-1.0);
        assert!(matches!(
            validate_arguments(&arguments),
            Err(CliError::InvalidArguments(_))
        ));
    }
}
