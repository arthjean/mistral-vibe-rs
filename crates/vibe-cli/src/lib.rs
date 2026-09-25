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

pub mod argv;
mod bootstrap;
pub mod distribution;
pub mod mcp_command;
mod programmatic;
pub mod tui;

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use clap::{ArgAction, Parser, ValueEnum};
use serde_json::Value;
use thiserror::Error;
use vibe_app_server::client::{ClientError, TurnDriver};
use vibe_app_server::server::AppServer;
use vibe_app_server::workspace::WorkspaceService;
use vibe_core::auth::KeyringStore;
use vibe_core::observability::{self, init_file_logging};
use vibe_core::telemetry::{
    ClientTelemetry, ExperimentExposures, LaunchContext, ReqwestTelemetryTransport,
    TelemetryClient, TelemetryConfig, TelemetryConfigGetter, TelemetryContext,
    TelemetryEventObserver, TelemetryRecord, detect_terminal_emulator,
};
use vibe_core::tracing::{TracingGuard, TracingSetup, setup_tracing};
use vibe_core::{engine::EventObserver, events::EventEnvelope};

#[derive(Debug, Clone, Parser)]
#[command(
    name = "vibe",
    version,
    disable_help_flag = true,
    disable_version_flag = true,
    // argparse keeps the last occurrence of an option given twice, flags
    // included, where clap refuses the second one.
    args_override_self = true,
    about = "Run the Mistral Vibe interactive CLI",
    after_help = EPILOG
)]
pub struct Arguments {
    // The declaration order below is the reference's own
    // (`vibe/cli/entrypoint.py:41-179`), because both parsers render their
    // options in the order they were declared: `-h` is first upstream only
    // because argparse adds it before anything else, which is why this port
    // declares it rather than letting clap append its own at the end.
    #[arg(
        short = 'h',
        long = "help",
        action = ArgAction::Help,
        help = "Print this help text and exit"
    )]
    pub help: Option<bool>,
    #[arg(
        short = 'v',
        long = "version",
        action = ArgAction::Version,
        help = "Print the installed version and exit"
    )]
    pub version: Option<bool>,
    #[arg(
        value_name = "PROMPT",
        help = "Opening prompt, submitted as soon as the interactive session is ready"
    )]
    pub initial_prompt: Option<String>,
    #[arg(
        short = 'p',
        long,
        num_args = 0..=1,
        default_missing_value = "",
        value_name = "TEXT",
        help = "Run one programmatic turn on TEXT and exit. Given without a value, the prompt \
                is read from standard input."
    )]
    pub prompt: Option<String>,
    /// Read as Python's `int()` reads it, sign included, because the
    /// reference declares `type=int` (`vibe/cli/entrypoint.py:74-80`).
    #[arg(
        long,
        value_name = "N",
        value_parser = argv::python_int,
        help = "Stop the session after N assistant turns"
    )]
    pub max_turns: Option<i64>,
    #[arg(
        long,
        value_name = "DOLLARS",
        value_parser = argv::python_float,
        help = "Stop the session once its accumulated cost reaches DOLLARS"
    )]
    pub max_price: Option<f64>,
    #[arg(
        long,
        value_name = "N",
        value_parser = argv::python_int,
        help = "Stop the session once it has spent N tokens"
    )]
    pub max_tokens: Option<i64>,
    #[arg(
        long = "enabled-tools",
        action = ArgAction::Append,
        value_name = "TOOL",
        help = "Restrict the session to TOOL. Repeat the flag to allow several."
    )]
    pub enabled_tools: Vec<String>,
    #[arg(
        long = "disabled-tools",
        action = ArgAction::Append,
        value_name = "TOOL",
        help = "Withhold TOOL from the session. Repeat the flag to withhold several."
    )]
    pub disabled_tools: Vec<String>,
    #[arg(
        long,
        value_enum,
        default_value_t = OutputMode::Text,
        value_name = "{text,json,streaming}",
        help = "Shape of the programmatic output: text prints the final answer, json prints \
                one object once the turn ends, streaming prints one object per event."
    )]
    pub output: OutputMode,
    #[arg(
        long,
        value_name = "NAME",
        help = "Start the session under the agent called NAME"
    )]
    pub agent: Option<String>,
    /// The Unified Harness is a backend this port does not ship, so asking for
    /// it lands on the legacy harness with a startup notice, which is what the
    /// reference does when its own backend cannot be loaded
    /// (`vibe/app_server/_runtime.py:1149-1167`).
    #[arg(
        long,
        conflicts_with = "legacy_harness",
        help = "Ask for the Unified Harness backend. This build carries none, so the session \
                runs on the legacy harness and reports the fallback when it starts."
    )]
    pub experimental_harness: bool,
    #[arg(
        long,
        help = "Keep the session on the legacy harness whatever the rollout selects"
    )]
    pub legacy_harness: bool,
    #[arg(
        long,
        help = "Start in the smart-approve mode, which also asks for the Unified Harness. On \
                the legacy harness, tool calls are approved the ordinary way."
    )]
    pub smart_approve: bool,
    #[arg(
        long,
        visible_alias = "yolo",
        help = "Approve every tool call without asking"
    )]
    pub auto_approve: bool,
    #[arg(long, help = "Run the interactive setup and exit")]
    pub setup: bool,
    #[arg(
        long,
        action = ArgAction::SetTrue,
        help = "Report whether a newer release is available and exit"
    )]
    pub check_upgrade: bool,
    #[arg(
        long,
        value_name = "DIR",
        help = "Change to DIR before the session starts"
    )]
    pub workdir: Option<PathBuf>,
    /// Absent, bare, or naming the worktree. A bare `--worktree` is `Some(None)`
    /// and leaves the name to Vibe, which is argparse's `nargs="?"` with a
    /// `const` of `True` (`vibe/cli/entrypoint.py:158-169`); a value is read as
    /// the reference reads it, so an empty one asks for no worktree at all.
    #[arg(
        long,
        value_name = "NAME",
        num_args = 0..=1,
        help = "Run in a git worktree kept under the vibe home. With NAME, create the worktree \
                and a branch called NAME, or reuse the one already there. Without NAME, create \
                a new one named after the prompt (or a random slug) on a vibe/<name> branch. \
                The session trusts that directory without asking. Ignored with --setup and \
                --check-upgrade."
    )]
    pub worktree: Option<Option<String>>,
    #[arg(
        long = "add-dir",
        value_name = "DIR",
        help = "Give the session access to DIR alongside the working directory. Repeat the flag \
                to add several."
    )]
    pub add_directories: Vec<PathBuf>,
    #[arg(long, help = "Trust the workspace without asking")]
    pub trust: bool,
    #[arg(long, hide = true)]
    pub teleport: bool,
    #[arg(
        short = 'c',
        long = "continue",
        conflicts_with = "resume",
        help = "Continue the most recent session of this workspace"
    )]
    pub continue_session: bool,
    #[arg(
        long,
        conflicts_with = "continue_session",
        num_args = 0..=1,
        default_missing_value = "",
        value_name = "SESSION_ID",
        help = "Resume the session called SESSION_ID. Given without a value, an interactive \
                launch opens the session picker."
    )]
    pub resume: Option<String>,
    // Below this line are the arguments this port declares and the reference
    // does not. Every one of them is hidden, and the ledger names them.
    #[arg(long = "allowed-tool", hide = true)]
    pub tool_filters: Vec<String>,
    #[arg(long, default_value = "mistral", hide = true)]
    pub provider_style: String,
    #[arg(long, default_value = "mistral-medium-3.5", hide = true)]
    pub model: String,
    #[arg(long, default_value_t = 1.5, hide = true)]
    pub input_price: f64,
    #[arg(long, default_value_t = 7.5, hide = true)]
    pub output_price: f64,
    #[arg(long, default_value = "https://api.mistral.ai/v1", hide = true)]
    pub api_base: String,
    #[arg(long, default_value = "MISTRAL_API_KEY", hide = true)]
    pub credential_environment: String,
    #[arg(long, hide = true)]
    pub session_root: Option<PathBuf>,
    #[arg(long, hide = true)]
    pub fake_response: Option<String>,
}

/// The block the help closes with, carrying the reference's two headings and
/// the same names in the same order (`vibe/cli/entrypoint.py:26-40`). The
/// sentences are this repository's own.
const EPILOG: &str = "\
Commands:
  update   Look for a newer release now, as --check-upgrade does.
  mcp      Manage the MCP servers a session can reach.

Environment variables:
  VIBE_HOME       Directory holding the configuration, the sessions and the logs.
  LOG_LEVEL       Verbosity of the log file, from CRITICAL down to DEBUG.
  LOG_MAX_BYTES   Size at which the log file is rotated.
  VIBE_*          Any other VIBE_ variable overrides the setting of the same name.";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum OutputMode {
    #[default]
    Text,
    Json,
    Streaming,
}

/// Programmatic mode: `vibe -p`. See [`programmatic`].
pub async fn run(
    arguments: Arguments,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> Result<(), CliError> {
    programmatic::run(arguments, stdout, stderr).await
}

/// Runs one programmatic launch with `driver` in place of a provider.
pub async fn execute<D>(
    arguments: Arguments,
    driver: D,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> Result<(), CliError>
where
    D: TurnDriver,
{
    validate_arguments(&arguments)?;
    let mut interrupt = programmatic::Interrupt::install();
    programmatic::execute_with_server(
        arguments,
        driver,
        AppServer::default(),
        None,
        &mut interrupt,
        stdout,
        stderr,
    )
    .await
}

/// The price arguments only this port declares, which the reference would
/// have refused as unknown before anything else ran.
fn validate_launch_prices(arguments: &Arguments) -> Result<(), CliError> {
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

fn validate_arguments(arguments: &Arguments) -> Result<(), CliError> {
    // Reference `args.prompt or stdin_prompt`: an empty prompt is refused,
    // and a prompt of blanks is a prompt like any other.
    if arguments.prompt.as_deref() == Some("") {
        return Err(CliError::InvalidArguments(
            "No prompt provided for programmatic mode".to_owned(),
        ));
    }
    if arguments.resume.as_deref() == Some("") && arguments.prompt.is_some() {
        return Err(CliError::InvalidArguments(
            "--resume requires a session ID in programmatic mode".to_owned(),
        ));
    }
    validate_launch_prices(arguments)
}

fn price_per_million_micros(price: f64) -> Result<u64, CliError> {
    if !price.is_finite() || price < 0.0 || price > u64::MAX as f64 / 1_000_000.0 {
        return Err(CliError::InvalidArguments(
            "model pricing must be a finite non-negative number".to_owned(),
        ));
    }
    Ok((price * 1_000_000.0).round() as u64)
}

#[derive(Debug, Error)]
pub enum CliError {
    /// The reference prefixes every post-parse refusal with `Error: ` and
    /// exits 1, which is the exit code a wrapper reads to tell a usage error
    /// from a run that started and failed (`vibe/cli/cli.py:147-150`).
    #[error("Error: {0}")]
    InvalidArguments(String),
    #[error("cannot resolve current directory: {0}")]
    CurrentDirectory(std::io::Error),
    #[error("stdout write failed: {0}")]
    Stdout(std::io::Error),
    #[error("stderr write failed: {0}")]
    Stderr(std::io::Error),
    /// Reference `ProgrammaticLimitError`: the last thing the assistant said,
    /// alone on stderr.
    #[error("{0}")]
    Limit(String),
    /// Reference `AppServerTurnError`, caught as a `RuntimeError`.
    #[error("Error: {0}")]
    TurnFailed(String),
    /// Reference `ProgrammaticTeleportError`.
    #[error("Teleport error: {0}")]
    Teleport(String),
    /// Reference `AppServerResponseError`: the server refused the session or
    /// the turn.
    #[error("Error: {0}")]
    Session(String),
    /// Reference `MissingAPIKeyError`, reported by `require_api_key_or_onboard`
    /// for a launch that cannot onboard. The guidance after the first sentence
    /// is this port's own.
    #[error(
        "Error: Missing {variable} environment variable for {provider} provider. Export it, \
         add it to the .env file in your Vibe home, or store it once with `vibe --setup`."
    )]
    MissingApiKey { variable: String, provider: String },
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
        help: None,
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
        experimental_harness: false,
        legacy_harness: false,
        smart_approve: false,
        auto_approve: false,
        setup: false,
        check_upgrade: false,
        worktree: None,
        teleport: false,
        provider_style: "mistral".to_owned(),
        model: "mistral-medium-3.5".to_owned(),
        input_price: 1.5,
        output_price: 7.5,
        api_base: "https://api.mistral.ai/v1".to_owned(),
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
    workspace: &WorkspaceService,
    working_directory: &Path,
    trust: bool,
) -> vibe_core::telemetry::records::NewSession {
    let nb_skills = workspace
        .dispatch("skills/list", &BTreeMap::new())
        .ok()
        .and_then(|dispatch| dispatch.result.get("skills").cloned())
        .as_ref()
        .and_then(Value::as_array)
        .map_or(0, Vec::len) as u64;
    let nb_mcp_servers = workspace
        .mcp_servers_for_session(working_directory, trust, &[])
        .map_or(0, |servers| servers.len()) as u64;
    let nb_models = workspace
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
    /// The exposures every census reads, shared with the session that resolves
    /// them. Reference builds its client with a getter closed over the
    /// experiment manager; this is the same reading, through a handle.
    exposures: ExperimentExposures,
}

impl CliTelemetryObserver {
    /// The handle a resolved rollout publishes its confirmed exposures into.
    pub(crate) fn exposures(&self) -> ExperimentExposures {
        self.exposures.clone()
    }

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

/// The same client every terminal-side event travels through also carries the
/// ones a client recorded on the app server, which is what the reference does
/// by handing `telemetry/record` to the agent loop's own client.
impl ClientTelemetry for CliTelemetryObserver {
    fn record_client_event(
        &self,
        name: &str,
        properties: serde_json::Map<String, serde_json::Value>,
        session_id: Option<&str>,
        correlate_last_request: bool,
    ) {
        self.events
            .record_client_event(name, properties, session_id, correlate_last_request);
    }
}

/// What this binary reports about itself on every event. Reference
/// `_build_cli_launch_context`.
pub(crate) fn cli_launch_context() -> LaunchContext {
    LaunchContext {
        agent_entrypoint: "cli".to_owned(),
        agent_version: env!("CARGO_PKG_VERSION").to_owned(),
        client_name: "vibe_cli".to_owned(),
        client_version: env!("CARGO_PKG_VERSION").to_owned(),
        terminal_emulator: Some(detect_terminal_emulator().to_owned()),
    }
}

/// What a programmatic launch reports about itself. Reference
/// `_build_launch_context_from_services`, which reads the `ClientInfo`
/// `_run_programmatic_mode` declared.
pub(crate) fn programmatic_launch_context() -> LaunchContext {
    LaunchContext {
        agent_entrypoint: "programmatic".to_owned(),
        agent_version: env!("CARGO_PKG_VERSION").to_owned(),
        client_name: "vibe_programmatic".to_owned(),
        client_version: env!("CARGO_PKG_VERSION").to_owned(),
        terminal_emulator: Some(detect_terminal_emulator().to_owned()),
    }
}

fn cli_telemetry_context(
    launch: LaunchContext,
    exposures: ExperimentExposures,
) -> TelemetryContext {
    TelemetryContext {
        launch: Some(launch),
        experiments: exposures,
        ..TelemetryContext::default()
    }
}

/// How a variable becomes a credential for everything this binary resolves off
/// a provider: the same lookup telemetry uses, as the handle a detached task
/// can carry.
pub(crate) fn cli_credentials(arguments: &Arguments) -> vibe_app_server::experiments::Credentials {
    let resolve = telemetry_credentials(
        bootstrap::dotenv_values(arguments).environment(),
        KeyringStore::native(),
    );
    Arc::new(resolve)
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

/// Opens the log file this process writes to, at the reference path under the
/// home the invocation resolves.
///
/// Reference initializes file logging in its entrypoint before anything else
/// runs, so a failure that happens before the app server attaches still leaves
/// a line on disk. The degradation is reported once and the binary starts
/// anyway: an operator who cannot write a log still gets a session.
pub fn install_file_logging(arguments: &Arguments) {
    let working_directory = arguments
        .workdir
        .clone()
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));
    let home = tui::startup::vibe_home_directory(arguments, &working_directory);
    let path = home.join("logs").join("vibe.log");
    if let Err(error) = init_file_logging(&path, &|name| std::env::var(name).ok()) {
        eprintln!("Logging to {} is unavailable: {error}", path.display());
    }
}

/// Installs the span exporter this process exports through, when the merged
/// configuration asks for one.
///
/// Reference installs it once per process from the app-server runtime; this
/// port has no such latch because the guard is the lifetime: held here for the
/// whole run, it flushes what a batch is still holding when the binary exits.
/// An unreadable configuration installs nothing, matching the way telemetry
/// itself reads one.
#[must_use]
pub fn install_tracing(arguments: &Arguments) -> Option<TracingGuard> {
    let credentials = telemetry_credentials(
        bootstrap::dotenv_values(arguments).environment(),
        KeyringStore::native(),
    );
    let setup = match WorkspaceService::default().layered_config().load() {
        Ok(snapshot) => setup_tracing(&snapshot.effective, &credentials),
        Err(_) => TracingSetup::Disabled,
    };
    match setup {
        TracingSetup::Installed(guard) => Some(guard),
        TracingSetup::UnusableEndpoint { endpoint } => {
            report_degradation(&format!(
                "OTEL tracing is enabled but `{endpoint}` is not a usable collector; skipping."
            ));
            None
        }
        TracingSetup::MissingCredential { variable } => {
            // Reference logs the same warning and starts anyway.
            report_degradation(&format!(
                "OTEL tracing is enabled but {variable} is not set; skipping."
            ));
            None
        }
        TracingSetup::Disabled => None,
    }
}

/// A startup degradation, told to the operator on the terminal and left on disk
/// for the one who reads the log afterward. Reference warns through the same
/// logger the file handler is attached to.
fn report_degradation(message: &str) {
    eprintln!("{message}");
    observability::log(observability::LogLevel::Warning, message);
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
    workspace: &WorkspaceService,
) -> Result<Arc<CliTelemetryObserver>, CliError> {
    telemetry_observer_for(arguments, workspace, cli_launch_context())
}

/// [`telemetry_observer`] for a launch that reports itself as `launch`.
pub(crate) fn telemetry_observer_for(
    arguments: &Arguments,
    workspace: &WorkspaceService,
    launch: LaunchContext,
) -> Result<Arc<CliTelemetryObserver>, CliError> {
    let configuration = workspace.layered_config();
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
    let exposures = ExperimentExposures::default();
    Ok(Arc::new(CliTelemetryObserver {
        events: TelemetryEventObserver::new(
            TelemetryClient::new(config, transport),
            cli_telemetry_context(launch, exposures.clone()),
        ),
        exposures,
    }))
}

impl CliError {
    #[must_use]
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::InvalidArguments(_) => 1,
            _ => 1,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io;

    use super::*;
    use vibe_app_server::client::{DriverFuture, EchoTurnDriver, TurnReservation};

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
            help: None,
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
            experimental_harness: false,
            legacy_harness: false,
            smart_approve: false,
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

    /// US-017: the binary opens the reference path under the home the
    /// invocation resolves, before anything else can fail, and a second call
    /// attaches nothing new. The installation is process-wide, so this is the
    /// one test that owns it.
    #[test]
    fn the_binary_opens_the_log_file_under_the_home_it_resolves() {
        let enclosure = tempfile::tempdir().expect("a home enclosure");
        let home = enclosure.path().join(".vibe");
        let mut invocation = arguments(OutputMode::Text);
        invocation.session_root = Some(home.join("sessions"));
        install_file_logging(&invocation);

        let installed = observability::installed_log().expect("a log file is installed");
        assert_eq!(installed.path(), home.join("logs").join("vibe.log"));
        assert!(installed.path().is_file(), "the file opens at startup");

        observability::log(observability::LogLevel::Critical, "the oracle was here");
        let page = installed.reader().get_logs(10, 0);
        assert_eq!(
            page.entries
                .first()
                .map(|entry| entry.message.clone())
                .unwrap_or_default(),
            "the oracle was here",
            "a record written through the installed logger reads back"
        );

        // A second initialization attaches nothing: the file stays the one
        // already open, which is the guard the reference keeps per path.
        install_file_logging(&invocation);
        assert_eq!(
            observability::installed_log().map(observability::FileLog::path),
            Some(home.join("logs").join("vibe.log").as_path())
        );
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
        assert!(
            validate_arguments(&arguments).is_ok(),
            "a blank prompt runs"
        );
        arguments.prompt = Some(String::new());
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
        let context = cli_telemetry_context(cli_launch_context(), ExperimentExposures::default());
        let launch = context.launch.expect("the CLI declares a launch context");
        assert_eq!(launch.agent_entrypoint, "cli");
        assert_eq!(launch.client_name, "vibe_cli");
        assert_eq!(launch.agent_version, env!("CARGO_PKG_VERSION"));
        assert_eq!(launch.client_version, env!("CARGO_PKG_VERSION"));
        assert!(launch.terminal_emulator.is_some());
    }

    /// The census reads the exposures through the handle rather than through a
    /// value taken at construction, which is what lets a rollout resolved after
    /// the client is built reach the next event. Reference builds its client
    /// with a getter closed over the experiment manager for the same reason.
    #[test]
    fn a_rollout_resolved_after_the_client_reaches_the_next_event() {
        let exposures = ExperimentExposures::default();
        let context = cli_telemetry_context(cli_launch_context(), exposures.clone());
        assert!(
            !context
                .base_metadata(None)
                .properties()
                .contains_key("experiments"),
            "an unenrolled session reports no field at all"
        );
        exposures.publish(BTreeMap::from([(
            "vibe_cli_system_prompt".to_owned(),
            "lean".to_owned(),
        )]));
        assert_eq!(
            context.base_metadata(None).properties()["experiments"],
            serde_json::json!({"vibe_cli_system_prompt": "lean"})
        );
    }

    #[test]
    fn an_untrusted_folder_with_project_configuration_warns_on_stderr() {
        let root = tempfile::tempdir().expect("root");
        let home = root.path().join("vibe-home");
        let project = root.path().join("project");
        std::fs::create_dir_all(project.join(".vibe")).expect("project");
        std::fs::write(project.join(".vibe/config.toml"), "").expect("config");
        std::fs::write(project.join("AGENTS.md"), "").expect("agents");
        let mut stderr = Vec::new();
        programmatic::warn_if_workspace_untrusted(&home, &project, &mut stderr).expect("warned");
        let warning = String::from_utf8(stderr).expect("UTF-8");
        assert!(warning.contains("(.vibe/, AGENTS.md)"), "{warning}");

        let store = vibe_core::trust::TrustStore::for_vibe_home(&home);
        store.trust_for_session(&project);
        let mut stderr = Vec::new();
        programmatic::warn_if_workspace_untrusted(&home, &project, &mut stderr).expect("silent");
        store.revoke_session_trust(&project);
        assert!(stderr.is_empty(), "a --trust run is not warned");
    }

    #[tokio::test]
    async fn text_json_and_streaming_have_deterministic_channels() {
        for (mode, expected) in [
            (OutputMode::Text, "world\n"),
            (OutputMode::Json, "\"role\": \"assistant\""),
            (OutputMode::Streaming, "\"role\": \"assistant\""),
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

    /// The programmatic entry point declares its session headless and
    /// withholds the two tools a question would have to reach a human through.
    ///
    /// Both are read off the reservation the server hands the driver, so what
    /// is asserted is what the session actually opened with rather than what
    /// the options struct was filled with. The reference sets the same pair on
    /// this branch and neither on the interactive one
    /// (`vibe/cli/cli.py:151-192` against `:209-272`).
    #[tokio::test]
    async fn a_programmatic_run_opens_a_headless_session_without_the_interactive_tools() {
        struct IntentRecordingDriver {
            inner: EchoTurnDriver,
            seen: Arc<std::sync::Mutex<Option<vibe_app_server::server::SessionIntent>>>,
        }

        impl TurnDriver for IntentRecordingDriver {
            fn run<'a>(&'a self, reservation: &'a TurnReservation) -> DriverFuture<'a> {
                if let Ok(mut seen) = self.seen.lock() {
                    *seen = Some(reservation.intent.clone());
                }
                self.inner.run(reservation)
            }
        }

        // The allowlist names one of the two withheld tools, which is the case
        // the denylist has to stay final for.
        let mut arguments = arguments(OutputMode::Text);
        arguments.enabled_tools = vec!["ask_user_question".to_owned(), "read_file".to_owned()];
        arguments.disabled_tools = vec!["shell".to_owned(), "exit_plan_mode".to_owned()];
        let seen = Arc::new(std::sync::Mutex::new(None));
        let driver = IntentRecordingDriver {
            inner: EchoTurnDriver::new("world"),
            seen: Arc::clone(&seen),
        };
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        execute(arguments, driver, &mut stdout, &mut stderr)
            .await
            .expect("programmatic run");

        let intent = seen
            .lock()
            .expect("recorded intent")
            .clone()
            .expect("the turn was reserved");
        assert!(intent.headless, "the programmatic session is headless");
        assert!(
            intent.disabled_tools.contains(&"shell".to_owned()),
            "the user's own list survives: {:?}",
            intent.disabled_tools
        );
        for withheld in ["ask_user_question", "exit_plan_mode"] {
            assert_eq!(
                intent
                    .disabled_tools
                    .iter()
                    .filter(|name| *name == withheld)
                    .count(),
                1,
                "{withheld} is withheld exactly once: {:?}",
                intent.disabled_tools
            );
        }
        assert!(
            intent
                .enabled_tools
                .contains(&"ask_user_question".to_owned()),
            "the allowlist is left as the user wrote it: {:?}",
            intent.enabled_tools
        );
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

    /// The reference types `--max-price` as a plain float and compares it, so
    /// a negative budget is a budget already spent rather than a refusal.
    #[test]
    fn a_negative_price_budget_is_accepted_as_it_was_given() {
        let mut arguments = arguments(OutputMode::Text);
        arguments.max_price = Some(-1.0);
        assert!(validate_arguments(&arguments).is_ok());
        let budgets = bootstrap::Budgets::of(&arguments);
        assert_eq!(budgets.max_price_micros, Some(-1_000_000));
    }

    /// Reference `run_cli` prints a missing key after `Error: ` and exits 1,
    /// the code every other programmatic failure exits with.
    #[test]
    fn a_missing_key_is_an_error_line_and_exit_one() {
        let error = CliError::MissingApiKey {
            variable: "MISTRAL_API_KEY".to_owned(),
            provider: "mistral".to_owned(),
        };
        assert!(
            error
                .to_string()
                .starts_with("Error: Missing MISTRAL_API_KEY ")
        );
        assert_eq!(error.exit_code(), 1);
    }
}

#[cfg(test)]
mod cli_surface_parity_tests;
