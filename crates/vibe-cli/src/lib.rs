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

mod argparse;
pub mod argv;
mod bootstrap;
pub mod distribution;
pub mod mcp_command;
mod programmatic;
pub mod tui;
pub mod update_notifier;

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use clap::{ArgAction, Parser, ValueEnum};
use thiserror::Error;
use vibe_app_server::client::{ClientError, TurnDriver};
use vibe_app_server::server::AppServer;
use vibe_app_server::workspace::WorkspaceService;
#[cfg(test)]
pub(crate) use vibe_app_server::workspace::has_agents_md;
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
    /// Reference `add_experimental_harness_argument`
    /// (`vibe/_experimental_harness.py`): the Unified Harness is already the
    /// default, so the flag only restates it.
    #[arg(
        long,
        conflicts_with = "legacy_harness",
        help = "Run on the Unified Harness backend. Every session already does by default; \
                the flag stays until the legacy harness is retired."
    )]
    pub experimental_harness: bool,
    #[arg(
        long,
        help = "Run on the legacy harness instead of the Unified Harness. A temporary way \
                out, kept until the legacy backend is removed."
    )]
    pub legacy_harness: bool,
    #[arg(
        long,
        help = "Sort each tool call by risk: run the safe ones on their own and ask only \
                about the risky ones."
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
    // does not. Every one of them is hidden, and the ledger names them. The
    // provider and price arguments default nowhere here: one left out lets the
    // configuration decide, which a value equal to the default does not, and
    // the accessors below answer the value a launch runs with.
    #[arg(long = "allowed-tool", hide = true)]
    pub tool_filters: Vec<String>,
    #[arg(long, hide = true)]
    pub provider_style: Option<String>,
    #[arg(long, hide = true)]
    pub model: Option<String>,
    #[arg(long, hide = true)]
    pub input_price: Option<f64>,
    #[arg(long, hide = true)]
    pub output_price: Option<f64>,
    #[arg(long, hide = true)]
    pub api_base: Option<String>,
    #[arg(long, hide = true)]
    pub credential_environment: Option<String>,
    #[arg(long, hide = true)]
    pub session_root: Option<PathBuf>,
    #[arg(long, hide = true)]
    pub fake_response: Option<String>,
}

impl Arguments {
    /// `--provider-style`, or the Mistral style a launch that names none speaks.
    #[must_use]
    pub fn provider_style(&self) -> &str {
        self.provider_style
            .as_deref()
            .unwrap_or(bootstrap::DEFAULT_PROVIDER_STYLE)
    }

    /// `--model`, or the model a launch that names none starts from.
    #[must_use]
    pub fn model(&self) -> &str {
        self.model.as_deref().unwrap_or(tui::DEFAULT_MODEL)
    }

    /// `--input-price`, per million tokens.
    #[must_use]
    pub fn input_price(&self) -> f64 {
        self.input_price.unwrap_or(bootstrap::DEFAULT_INPUT_PRICE)
    }

    /// `--output-price`, per million tokens.
    #[must_use]
    pub fn output_price(&self) -> f64 {
        self.output_price.unwrap_or(bootstrap::DEFAULT_OUTPUT_PRICE)
    }

    /// `--api-base`, or the Mistral endpoint.
    #[must_use]
    pub fn api_base(&self) -> &str {
        self.api_base
            .as_deref()
            .unwrap_or(bootstrap::DEFAULT_API_BASE)
    }

    /// `--credential-environment`, or the variable the Mistral key is read from.
    #[must_use]
    pub fn credential_environment(&self) -> &str {
        self.credential_environment
            .as_deref()
            .unwrap_or(bootstrap::DEFAULT_CREDENTIAL_ENVIRONMENT)
    }
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
        ("input-price", arguments.input_price()),
        ("output-price", arguments.output_price()),
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
        provider_style: None,
        model: None,
        input_price: None,
        output_price: None,
        api_base: None,
        credential_environment: None,
        session_root: None,
        fake_response: None,
    }
}

/// The census `vibe.new_session` reports for a session opened in
/// `working_directory`.
pub(crate) fn session_census(
    workspace: &WorkspaceService,
    working_directory: &Path,
    trust: bool,
) -> vibe_core::telemetry::records::NewSession {
    workspace.session_census(working_directory, trust)
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

    /// The census every event carries, which every request reports too.
    pub(crate) fn context(&self) -> vibe_core::telemetry::TelemetryContext {
        self.events.context().clone()
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

    fn record(&self, record: &TelemetryRecord, session_id: Option<&str>) {
        let _ = self.enqueue(record, session_id);
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

/// What the interactive client reports about itself once its session runs.
/// Reference `run_cli` declares the `vibe_tui` client to the harness, and the
/// census reads the client descriptor (`_build_launch_context_from_services`);
/// [`cli_launch_context`] stays what onboarding reports, as the reference's
/// own `_build_cli_launch_context` does.
pub(crate) fn tui_launch_context() -> LaunchContext {
    LaunchContext {
        client_name: "vibe_tui".to_owned(),
        ..cli_launch_context()
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
    use_unified: bool,
) -> TelemetryContext {
    TelemetryContext {
        launch: Some(launch),
        experiments: exposures,
        harness_backend: Some(vibe_core::telemetry::harness_backend(use_unified).to_owned()),
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
/// Reference `load_dotenv_values`, run at startup by `vibe/cli/entrypoint.py`:
/// the variables `{vibe_home}/.env` declares become part of the environment
/// every child process inherits.
pub fn publish_dotenv(arguments: &Arguments) {
    bootstrap::dotenv_values(arguments).publish_to_children();
}

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
    let credentials = cli_credentials(arguments);
    let setup = match WorkspaceService::default().layered_config().load() {
        Ok(snapshot) => setup_tracing(&snapshot.effective, credentials.as_ref()),
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
    let credentials = cli_credentials(arguments);
    let config: TelemetryConfigGetter = Arc::new(move || match configuration.load() {
        Ok(snapshot) => TelemetryConfig::resolve(&snapshot.effective, credentials.as_ref()),
        Err(_) => TelemetryConfig::disabled(),
    });
    let transport = ReqwestTelemetryTransport::try_new()
        .map_err(|error| CliError::Telemetry(error.to_string()))?;
    let exposures = ExperimentExposures::default();
    Ok(Arc::new(CliTelemetryObserver {
        events: TelemetryEventObserver::new(
            TelemetryClient::new(config, transport),
            cli_telemetry_context(
                launch,
                exposures.clone(),
                vibe_app_server::harness::HarnessSelection::resolve(
                    arguments.experimental_harness,
                    arguments.legacy_harness,
                )
                .use_unified,
            ),
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
mod lib_tests;

#[cfg(test)]
#[path = "../tests/support/test_root.rs"]
mod test_root;

#[cfg(test)]
mod cli_surface_parity_tests;
