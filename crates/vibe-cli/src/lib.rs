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

mod bootstrap;
pub mod distribution;
pub mod mcp_command;
pub mod tui;

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use clap::{ArgAction, Parser, ValueEnum};
use serde::Serialize;
use serde_json::Value;
use thiserror::Error;
use vibe_app_server::client::{
    ClientError, HeadlessService, LiveTurnDriver, ProgrammaticTeleportEvent, ProgrammaticTurn,
    ProgrammaticUpdate, PublicTurnStopReason, TurnDriver, programmatic_update_channel,
};
use vibe_app_server::release3::Release3Service;
use vibe_app_server::server::AppServer;
use vibe_core::auth::KeyringStore;
use vibe_core::mcp::SamplingHandler;
use vibe_core::telemetry::{
    LaunchContext, ReqwestTelemetryTransport, TelemetryClient, TelemetryConfig,
    TelemetryConfigGetter, TelemetryContext, TelemetryEventObserver, TelemetryRecord,
    detect_terminal_emulator,
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
        let config = bootstrap::live_driver_config(
            &arguments,
            &arguments.model,
            Release3Service::default().compaction_prompts(),
        )?;
        let credential = bootstrap::credential(&arguments)?;
        let release3 = Release3Service::default();
        // The programmatic entry point starts here, so this is where an older
        // configuration file is brought forward.
        release3
            .migrate_configuration()
            .map_err(|error| CliError::Configuration(error.to_string()))?;
        let telemetry = telemetry_observer(&arguments, &release3)?;
        // The server takes ownership of the service, and the session census is
        // read off the same one.
        let census_service = release3.clone();
        let mut driver = LiveTurnDriver::from_credential(config, credential)?;
        driver = driver.with_event_observer(telemetry.clone());
        let server = production_server(
            &arguments,
            release3,
            Some(driver.sampling_handler(&arguments.model)),
        )?;
        let census = arguments
            .workdir
            .clone()
            .or_else(|| std::env::current_dir().ok())
            .map(|working_directory| {
                session_census(&census_service, &working_directory, arguments.trust)
            });
        let result = execute_with_server(
            arguments,
            driver,
            server,
            Some(SessionTelemetry {
                observer: telemetry.clone(),
                census,
            }),
            stdout,
            stderr,
        )
        .await;
        telemetry.flush().await;
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
    execute_with_server(
        arguments,
        driver,
        AppServer::default(),
        None,
        stdout,
        stderr,
    )
    .await
}

/// What the programmatic path reports about the session it opens: the observer
/// every event goes to, and the census `vibe.new_session` carries.
struct SessionTelemetry {
    observer: Arc<CliTelemetryObserver>,
    census: Option<vibe_core::telemetry::records::NewSession>,
}

async fn execute_with_server<D>(
    arguments: Arguments,
    driver: D,
    server: AppServer,
    telemetry: Option<SessionTelemetry>,
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
    let working_directory = match arguments.workdir.clone() {
        Some(path) => path,
        None => std::env::current_dir().map_err(CliError::CurrentDirectory)?,
    };
    let options = bootstrap::session_options(
        &arguments,
        &working_directory,
        arguments.model.clone(),
        None,
        None,
    );
    let mut service = HeadlessService::new_shared_with_server(Arc::new(driver), server)?;
    let session_id = service.start_session(&options)?;
    // Reference `emit_new_session_telemetry` and `emit_ready_telemetry`: the
    // agent loop raises both once its initialization settles, whichever
    // entrypoint launched it.
    if let Some(telemetry) = telemetry.as_ref() {
        if let Some(census) = telemetry.census.clone() {
            let _ = telemetry
                .observer
                .enqueue(&TelemetryRecord::NewSession(census), Some(&session_id));
        }
        let _ = telemetry.observer.enqueue(
            &TelemetryRecord::Ready {
                init_duration_ms: since_process_start_ms(),
            },
            Some(&session_id),
        );
    }
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
    // Reference `emit_session_closed_telemetry`, raised before the session is
    // closed so the census still names it.
    if let Some(telemetry) = telemetry.as_ref() {
        let _ = telemetry
            .observer
            .enqueue(&TelemetryRecord::SessionClosed, Some(&close_session_id));
    }
    let close_result = service.close_session(&close_session_id).await;
    let shutdown_result = service.shutdown();
    execution?;
    close_result?;
    shutdown_result?;
    Ok(())
}

fn production_server(
    arguments: &Arguments,
    release3: Release3Service,
    sampling: Option<Arc<dyn SamplingHandler>>,
) -> Result<AppServer, CliError> {
    let credential = bootstrap::credential(arguments)?;
    let server = bootstrap::resource_server(arguments, release3, credential.clone(), sampling)?;
    if !arguments.teleport {
        return Ok(server);
    }
    Ok(server.using_release4_service(bootstrap::cloud_service(credential)?))
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
    #[error("configuration could not be prepared: {0}")]
    Configuration(String),
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

/// Reference `emit_new_session_telemetry`'s four counts, read off the same
/// services a session is built from: the workspace instructions file, every
/// discovered skill, the MCP servers this session would connect and the models
/// the merged configuration declares.
pub(crate) fn session_census(
    release3: &Release3Service,
    working_directory: &Path,
    trust: bool,
) -> vibe_core::telemetry::records::NewSession {
    let nb_skills = release3
        .dispatch("skills/list", &BTreeMap::new())
        .ok()
        .and_then(|dispatch| dispatch.result.get("skills").cloned())
        .as_ref()
        .and_then(Value::as_array)
        .map_or(0, Vec::len) as u64;
    let nb_mcp_servers = release3
        .mcp_servers_for_session(working_directory, trust, &[])
        .map_or(0, |servers| servers.len()) as u64;
    let nb_models = release3
        .layered_config()
        .load()
        .ok()
        .and_then(|snapshot| snapshot.effective.get("models").cloned())
        .map_or(0, |models| match models {
            toml::Value::Array(entries) => entries.len(),
            toml::Value::Table(entries) => entries.len(),
            _ => 0,
        }) as u64;
    vibe_core::telemetry::records::NewSession {
        has_agents_md: has_agents_md(working_directory),
        nb_skills,
        nb_mcp_servers,
        nb_models,
    }
}

/// Reference `has_agents_md_file`: the workspace publishes instructions to the
/// agent under either spelling.
#[must_use]
pub(crate) fn has_agents_md(working_directory: &Path) -> bool {
    ["AGENTS.md", "VIBE.md"]
        .into_iter()
        .any(|name| working_directory.join(name).is_file())
}

/// Reference `PROCESS_START_MONOTONIC`: what the startup durations are
/// measured from. `mark_process_start` fixes it at the top of `main`; a caller
/// that never marks it reads it at the first measurement, which is the same
/// reading the reference takes when its module is imported late.
static PROCESS_START: std::sync::LazyLock<std::time::Instant> =
    std::sync::LazyLock::new(std::time::Instant::now);

/// Fixes the process start reading. Idempotent.
pub fn mark_process_start() {
    let _ = *PROCESS_START;
}

/// How long ago the process started, in milliseconds.
#[must_use]
pub(crate) fn since_process_start_ms() -> u64 {
    u64::try_from(PROCESS_START.elapsed().as_millis()).unwrap_or(u64::MAX)
}

pub(crate) struct CliTelemetryObserver {
    events: TelemetryEventObserver<ReqwestTelemetryTransport>,
}

impl CliTelemetryObserver {
    /// Queues best-effort telemetry for the rating prompt. Delivery errors
    /// follow the same intentionally silent policy as engine telemetry, the
    /// gate is re-read on the send, and all queued work is joined by
    /// [`Self::flush`].
    ///
    /// The session travels with it: the reference sends this event through the
    /// agent loop's own client, whose census reports the session every event is
    /// recorded on.
    pub(crate) fn enqueue_feedback(
        &self,
        rating: u8,
        model: &str,
        session_id: &str,
    ) -> Result<(), String> {
        self.enqueue(
            &TelemetryRecord::FeedbackSubmitted {
                rating: u64::from(rating),
                model: model.to_owned(),
            },
            Some(session_id),
        )
    }

    /// Queues one event a client surface raised: a slash command, a copied
    /// selection, a cancelled action, an inserted mention, the voice toggle,
    /// the audio managers or the teleport tracker.
    ///
    /// The reference hands each of these to the agent loop's own telemetry
    /// client, so they carry the same census and the same gate as an event the
    /// turn itself produced.
    pub(crate) fn enqueue(
        &self,
        record: &TelemetryRecord,
        session_id: Option<&str>,
    ) -> Result<(), String> {
        self.events
            .record(record, session_id)
            .map_err(|error| error.to_string())
    }

    pub(crate) async fn flush(&self) {
        self.events.flush().await;
    }
}

impl EventObserver for CliTelemetryObserver {
    fn observe(&self, event: &EventEnvelope) -> Result<(), String> {
        self.events.observe(event)
    }
}

/// What this binary reports about itself on every event. Reference
/// `_build_cli_launch_context`.
fn cli_telemetry_context() -> TelemetryContext {
    TelemetryContext {
        launch: Some(LaunchContext {
            agent_entrypoint: "cli".to_owned(),
            agent_version: env!("CARGO_PKG_VERSION").to_owned(),
            client_name: "vibe_cli".to_owned(),
            client_version: env!("CARGO_PKG_VERSION").to_owned(),
            terminal_emulator: Some(detect_terminal_emulator().to_owned()),
        }),
        ..TelemetryContext::default()
    }
}

/// How the variable a Mistral provider names becomes a credential.
///
/// Reference `resolve_api_key`, reached from `get_mistral_provider_and_api_key`:
/// the process environment the dotenv load leaves behind first, then the OS
/// keyring under the shared service names. Reading the environment alone would
/// silence telemetry on every install whose key lives only in the keyring,
/// which is where onboarding puts it when the store accepts the write.
fn telemetry_credentials(
    environment: BTreeMap<String, String>,
    store: KeyringStore,
) -> impl Fn(&str) -> Option<String> + Send + Sync {
    move |name| vibe_core::auth::resolve_api_key(name, &environment, &store)
}

/// The observer every engine event reaches.
///
/// Nothing here decides whether telemetry is on: [`TelemetryConfig::resolve`]
/// re-reads `enable_telemetry` and the Mistral provider from the merged
/// configuration on every send, which is what makes a document edited
/// mid-session decide the next event and an unreadable one silence telemetry
/// rather than fail the run.
pub(crate) fn telemetry_observer(
    arguments: &Arguments,
    release3: &Release3Service,
) -> Result<Arc<CliTelemetryObserver>, CliError> {
    let configuration = release3.layered_config();
    let credentials = telemetry_credentials(
        bootstrap::dotenv_values(arguments).environment(),
        KeyringStore::native(),
    );
    let config: TelemetryConfigGetter = Arc::new(move || match configuration.load() {
        Ok(snapshot) => TelemetryConfig::resolve(&snapshot.effective, &credentials),
        Err(_) => TelemetryConfig::disabled(),
    });
    let transport = ReqwestTelemetryTransport::try_new()
        .map_err(|error| CliError::Telemetry(error.to_string()))?;
    Ok(Arc::new(CliTelemetryObserver {
        events: TelemetryEventObserver::new(
            TelemetryClient::new(config, transport),
            cli_telemetry_context(),
        ),
    }))
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

    /// The reference publishes no telemetry flag: the configuration key is the
    /// only control, so passing one is an unknown argument and no help output
    /// mentions it.
    #[test]
    fn the_binary_publishes_no_telemetry_flag() {
        use clap::CommandFactory;

        assert!(Arguments::try_parse_from(["vibe", "--telemetry"]).is_err());
        let help = Arguments::command().render_long_help().to_string();
        assert!(!help.contains("--telemetry"), "{help}");
    }

    /// Reference `resolve_api_key`: the credential a delivery authenticates
    /// with is read from the environment first and the OS keyring second, so a
    /// key onboarding stored in the keyring alone still activates telemetry.
    #[test]
    fn the_telemetry_credential_reaches_the_keyring() {
        use vibe_core::auth::{KEYRING_SERVICE, KeyringBackend, KeyringFailure};

        struct StoredKey;

        impl KeyringBackend for StoredKey {
            fn get(&self, service: &str, account: &str) -> Result<Option<String>, KeyringFailure> {
                Ok((service == KEYRING_SERVICE && account == "MISTRAL_API_KEY")
                    .then(|| "keyring-credential".to_owned()))
            }

            fn set(&self, _: &str, _: &str, _: &str) -> Result<(), KeyringFailure> {
                Err(KeyringFailure::NoBackend)
            }

            fn delete(&self, _: &str, _: &str) -> Result<(), KeyringFailure> {
                Err(KeyringFailure::NoEntry)
            }
        }

        let stored = || KeyringStore::new(Box::new(StoredKey));
        let credentials = telemetry_credentials(BTreeMap::new(), stored());
        assert_eq!(
            credentials("MISTRAL_API_KEY").as_deref(),
            Some("keyring-credential"),
            "a key held only by the credential store still resolves"
        );
        assert_eq!(credentials("ABSENT_KEY"), None);

        let exported = telemetry_credentials(
            BTreeMap::from([("MISTRAL_API_KEY".to_owned(), "exported".to_owned())]),
            stored(),
        );
        assert_eq!(
            exported("MISTRAL_API_KEY").as_deref(),
            Some("exported"),
            "the environment still wins over the store"
        );
    }

    /// The launch context is the reference's: the `cli` entrypoint, this
    /// build's version on both sides, and a terminal named from the published
    /// vocabulary.
    #[test]
    fn the_launch_context_reports_the_cli_entrypoint() {
        let context = cli_telemetry_context();
        let launch = context.launch.expect("the CLI declares a launch context");
        assert_eq!(launch.agent_entrypoint, "cli");
        assert_eq!(launch.client_name, "vibe_cli");
        assert_eq!(launch.agent_version, env!("CARGO_PKG_VERSION"));
        assert_eq!(launch.client_version, env!("CARGO_PKG_VERSION"));
        assert!(launch.terminal_emulator.is_some());
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
