//! Programmatic mode: `vibe -p` sends one prompt, prints what the session
//! produced and exits.
//!
//! Reference `run_programmatic` and `ProgrammaticOutput`
//! (`vibe/cli/programmatic.py`), launched by `_run_programmatic_mode`
//! (`vibe/cli/cli.py`). The session is opened the way the reference's
//! `LocalHarness` opens it: a client that declares approval and user-input
//! callbacks, and answers every one of them with a refusal, since nobody is
//! there to ask. What is printed is read off the session's own history, so
//! `json` is every entry of the session, `streaming` each entry once it
//! completes, and `text` the last thing the assistant said.

mod layout;
mod pyjson;

use std::collections::HashSet;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use vibe_app_server::client::{
    CallbackDetail, ClientCallbackKind, ClientCapabilities, ClientEntrypoint, ClientError,
    ClientInfo, DriverError, HeadlessService, ProgrammaticTeleportEvent, PublicCallbackState,
    PublicContentBlock, PublicHistoryEntry, PublicMessageRole, PublicTurnStopReason, TurnDriver,
    TurnErrorCode, TurnRequest, public_driver_error, public_turn_error,
};
use vibe_app_server::experiments::SessionExperiments;
use vibe_app_server::server::AppServer;
use vibe_app_server::workspace::WorkspaceService;
use vibe_core::telemetry::TelemetryRecord;

use self::pyjson::Ordered;
use crate::{
    Arguments, CliError, CliTelemetryObserver, OutputMode, bootstrap, since_process_start_ms,
};

/// How often a running turn is checked for a callback to refuse.
const CALLBACK_POLL: Duration = Duration::from_millis(20);

/// How long an interrupted turn is given to settle before the session closes
/// under it. Reference `_INTERRUPT_ON_CANCEL_TIMEOUT_SECONDS`.
const INTERRUPT_GRACE: Duration = Duration::from_secs(5);

/// Reference `_last_assistant_text`'s fallback when a limit stopped a turn
/// before the assistant said anything.
const LIMIT_FALLBACK: &str = "The configured conversation limit was reached";

/// What the programmatic path reports about the session it opens: the observer
/// every event goes to, the census `vibe.new_session` carries, and the
/// enrollment whose confirmed exposures every one of them reports.
pub(crate) struct SessionTelemetry {
    pub(crate) observer: Arc<CliTelemetryObserver>,
    pub(crate) census: Option<vibe_core::telemetry::records::NewSession>,
    pub(crate) experiments: Option<Arc<SessionExperiments>>,
}

/// The production launch: the configured model on its configured provider.
pub(crate) async fn run(
    arguments: Arguments,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> Result<(), CliError> {
    let mut interrupt = Interrupt::install();
    if let Some(response) = &arguments.fake_response {
        crate::validate_arguments(&arguments)?;
        return execute_with_server(
            arguments.clone(),
            vibe_app_server::client::EchoTurnDriver::new(response),
            AppServer::default(),
            None,
            &mut interrupt,
            stdout,
            stderr,
        )
        .await;
    }
    // Reference `run_cli`: the configuration loads and the active provider's
    // key is required before the programmatic arguments are looked at, so a
    // missing key is what an unusable launch reports first.
    crate::validate_launch_prices(&arguments)?;
    let workspace = WorkspaceService::default();
    workspace
        .migrate_configuration()
        .map_err(|error| CliError::Configuration(error.to_string()))?;
    let route = bootstrap::programmatic_route(&arguments, &workspace)?;
    let mut arguments = arguments;
    arguments.model.clone_from(&route.model);
    let credential = bootstrap::programmatic_credential(&arguments, &route.provider)?;
    crate::validate_arguments(&arguments)?;
    let config = bootstrap::route_driver_config(&route, &workspace)?;
    let telemetry = crate::telemetry_observer_for(
        &arguments,
        &workspace,
        crate::programmatic_launch_context(),
    )?;
    let census_service = workspace.clone();
    let mut driver =
        vibe_app_server::client::LiveTurnDriver::from_credential(config, credential.clone())?;
    driver = driver.with_event_observer(telemetry.clone());
    let mut server = bootstrap::route_resource_server(
        &arguments,
        workspace,
        &route,
        credential.clone(),
        Some(driver.sampling_handler(&route.model)),
    )?
    .using_client_telemetry(telemetry.clone());
    if arguments.teleport {
        server = server.using_projects_service(bootstrap::cloud_service(credential)?);
    }
    let census = arguments
        .workdir
        .clone()
        .or_else(|| std::env::current_dir().ok())
        .map(|working_directory| {
            crate::session_census(&census_service, &working_directory, arguments.trust)
        });
    // Reference builds the manager with the loop and starts the lookup as a
    // detached task once the session exists, so the programmatic path reports
    // the same enrollment an interactive one does.
    let experiments = Arc::new(SessionExperiments::new(
        &census_service,
        crate::cli_credentials(&arguments),
        Some(crate::programmatic_launch_context()),
        telemetry.exposures(),
    ));
    let result = execute_with_server(
        arguments,
        driver,
        server,
        Some(SessionTelemetry {
            observer: telemetry.clone(),
            census,
            experiments: Some(experiments),
        }),
        &mut interrupt,
        stdout,
        stderr,
    )
    .await;
    telemetry.flush().await;
    result
}

/// Runs one programmatic launch against `server`, turns driven by `driver`.
pub(crate) async fn execute_with_server<D>(
    arguments: Arguments,
    driver: D,
    server: AppServer,
    telemetry: Option<SessionTelemetry>,
    interrupt: &mut Interrupt,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> Result<(), CliError>
where
    D: TurnDriver,
{
    let prompt = arguments
        .prompt
        .clone()
        .or_else(|| arguments.initial_prompt.clone())
        .ok_or_else(|| {
            CliError::InvalidArguments("No prompt provided for programmatic mode".to_owned())
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
        bootstrap::Launch::Programmatic,
    );
    let mut service = HeadlessService::new_interactive_shared_with_server_and_client(
        Arc::new(driver),
        server,
        programmatic_client_info(),
        ClientCapabilities {
            callback_kinds: vec![ClientCallbackKind::Approval, ClientCallbackKind::UserInput],
            ..ClientCapabilities::default()
        },
    )?;
    let session_id = service
        .start_session(&options)
        .map_err(|error| CliError::Session(client_error_message(&error)))?;
    warn_if_workspace_untrusted(
        service.workspace_service().vibe_home(),
        &working_directory,
        stderr,
    )?;
    // Reference `emit_new_session_telemetry` and `emit_ready_telemetry`: the
    // agent loop raises both once its initialization settles, whichever
    // entrypoint launched it.
    if let Some(telemetry) = telemetry.as_ref() {
        if let Some(experiments) = telemetry.experiments.as_ref() {
            experiments.start(&session_id);
        }
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
    let mut output = Output::new(arguments.output);
    let mut run = Run {
        service: &mut service,
        session_id: session_id.clone(),
        interrupt,
    };
    let execution = run
        .execute(&arguments, &prompt, &working_directory, &mut output, stdout)
        .await;
    let close_session_id = run.session_id.clone();
    // Reference `aclose` cancels the experiments task before it closes
    // anything else, so a shutdown never waits on a lookup that is still going.
    if let Some(experiments) = telemetry
        .as_ref()
        .and_then(|telemetry| telemetry.experiments.as_ref())
    {
        experiments.close().await;
    }
    // Reference `emit_session_closed_telemetry`, raised before the session is
    // closed so the census still names it.
    if let Some(telemetry) = telemetry.as_ref() {
        let _ = telemetry
            .observer
            .enqueue(&TelemetryRecord::SessionClosed, Some(&close_session_id));
    }
    let close_result = service.close_session(&close_session_id).await;
    let shutdown_result = service.shutdown();
    let end = execution?;
    close_result?;
    shutdown_result?;
    match end {
        End::Finished(Some(response)) => {
            writeln!(stdout, "{response}").map_err(CliError::Stdout)?;
        }
        End::Finished(None) => {}
        // Reference `run_cli` answers a `KeyboardInterrupt` with a farewell on
        // stdout and a clean exit, whatever the session had printed.
        End::Interrupted => {
            write!(stdout, "\nBye!\n").map_err(CliError::Stdout)?;
        }
    }
    stdout.flush().map_err(CliError::Stdout)?;
    stderr.flush().map_err(CliError::Stderr)
}

/// Reference `ClientInfo(name="vibe_programmatic", ...)` in
/// `_run_programmatic_mode`, which is what the session's telemetry reports the
/// launch as.
fn programmatic_client_info() -> ClientInfo {
    ClientInfo {
        name: "vibe_programmatic".to_owned(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        title: Some("Vibe programmatic CLI".to_owned()),
        entrypoint: ClientEntrypoint::Programmatic,
        terminal_emulator: serde_json::from_value(json!(
            vibe_core::telemetry::detect_terminal_emulator()
        ))
        .unwrap_or_default(),
    }
}

/// The message an app-server refusal carries, which the reference prints
/// after `Error: ` (`AppServerResponseError` in `_run_programmatic_mode`).
fn client_error_message(error: &ClientError) -> String {
    match error {
        ClientError::Protocol(_, message) => message.clone(),
        ClientError::Server(server) => server.to_string(),
        other => other.to_string(),
    }
}

/// Reference `_warn_if_workspace_untrusted` (`vibe/cli/programmatic.py`): a
/// run in a folder nobody trusted, and that holds project configuration, says
/// on stderr that the configuration is skipped and how to lift that.
pub(crate) fn warn_if_workspace_untrusted(
    vibe_home: &Path,
    working_directory: &Path,
    stderr: &mut impl Write,
) -> Result<(), CliError> {
    let store = vibe_core::trust::TrustStore::for_vibe_home(vibe_home);
    let cwd = vibe_core::trust::resolve(working_directory);
    if store.trust_status(&cwd) != vibe_core::trust::TrustStatus::Untrusted {
        return Ok(());
    }
    let Some(prompt) = vibe_core::trust::build_trust_prompt(&cwd, true, &store) else {
        return Ok(());
    };
    let mut files: Vec<&str> = Vec::new();
    for file in prompt
        .detected_files
        .iter()
        .chain(&prompt.repo_detected_files)
    {
        if !files.contains(&file.as_str()) {
            files.push(file);
        }
    }
    if files.is_empty() {
        return Ok(());
    }
    writeln!(
        stderr,
        "Warning: {} is untrusted, so its project configuration ({}) is not loaded. \
         Pass --trust to trust this folder for this run.",
        prompt.cwd.display(),
        files.join(", ")
    )
    .map_err(CliError::Stderr)
}

/// How a launch ended when it did not fail.
enum End {
    /// The session answered; `text` mode prints the answer, if there is one.
    Finished(Option<String>),
    /// The operator pressed Ctrl-C.
    Interrupted,
}

/// SIGINT, taken over for the length of the launch so a Ctrl-C interrupts the
/// turn and closes the session instead of killing the process.
pub(crate) struct Interrupt {
    #[cfg(unix)]
    signal: Option<tokio::signal::unix::Signal>,
    #[cfg(windows)]
    signal: Option<tokio::signal::windows::CtrlC>,
    received: bool,
}

impl Interrupt {
    pub(crate) fn install() -> Self {
        #[cfg(unix)]
        let signal = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt()).ok();
        #[cfg(windows)]
        let signal = tokio::signal::windows::ctrl_c().ok();
        Self {
            signal,
            received: false,
        }
    }

    async fn recv(&mut self) {
        if self.received {
            return;
        }
        match self.signal.as_mut() {
            Some(signal) => {
                signal.recv().await;
                self.received = true;
            }
            None => std::future::pending().await,
        }
    }

    /// Whether a Ctrl-C already arrived, without waiting for one.
    fn pending(&mut self) -> bool {
        if self.received {
            return true;
        }
        let Some(signal) = self.signal.as_mut() else {
            return false;
        };
        let waker = std::task::Waker::noop();
        let mut context = std::task::Context::from_waker(waker);
        if signal.poll_recv(&mut context).is_ready() {
            self.received = true;
        }
        self.received
    }
}

/// One launch over an open session.
struct Run<'a, D: TurnDriver> {
    service: &'a mut HeadlessService<D>,
    session_id: String,
    interrupt: &'a mut Interrupt,
}

/// How a turn settled.
enum Settled {
    Finished(PublicTurnStopReason),
    Interrupted,
}

impl<D: TurnDriver> Run<'_, D> {
    async fn execute(
        &mut self,
        arguments: &Arguments,
        prompt: &str,
        working_directory: &Path,
        output: &mut Output,
        stdout: &mut impl Write,
    ) -> Result<End, CliError> {
        output.start(&self.history()?, stdout)?;
        if self.interrupt.pending() {
            return Ok(End::Interrupted);
        }
        if arguments.teleport {
            require_teleport_available(&self.service.workspace_service()).await?;
            let events = self
                .service
                .teleport(
                    &self.session_id,
                    &working_directory.to_string_lossy(),
                    prompt,
                    true,
                )
                .await
                .map_err(|error| teleport_refusal(&error))?;
            for event in &events {
                output.teleport(event, stdout)?;
                if let ProgrammaticTeleportEvent::Failed { error, .. } = event {
                    return Err(CliError::Teleport(error.message.clone()));
                }
            }
            return output.finalize(&self.history()?, stdout).map(End::Finished);
        }
        match self.turn(prompt, output, stdout).await? {
            Settled::Interrupted => Ok(End::Interrupted),
            Settled::Finished(stop_reason) => {
                let history = self.history()?;
                if matches!(
                    stop_reason,
                    PublicTurnStopReason::MaxSteps
                        | PublicTurnStopReason::TokenLimit
                        | PublicTurnStopReason::PriceLimit
                ) {
                    return Err(CliError::Limit(
                        last_assistant_text(&history).unwrap_or_else(|| LIMIT_FALLBACK.to_owned()),
                    ));
                }
                if let Some(error) = public_turn_error(&stop_reason) {
                    return Err(CliError::TurnFailed(error.message));
                }
                output.finalize(&history, stdout).map(End::Finished)
            }
        }
    }

    /// Runs the turn, refusing every callback it raises and streaming each
    /// entry as it completes. Reference `run_programmatic`'s `act` loop.
    async fn turn(
        &mut self,
        prompt: &str,
        output: &mut Output,
        stdout: &mut impl Write,
    ) -> Result<Settled, CliError> {
        let reservation = self
            .service
            .reserve_prompt(&self.session_id, &TurnRequest::text(prompt))
            .await
            .map_err(|error| CliError::Session(client_error_message(&error)))?;
        self.session_id.clone_from(&reservation.session_id);
        let (observer, mut updates) = match self.service.interactive_update_channel_after(
            &reservation.session_id,
            &reservation.turn_id,
            0,
        ) {
            Ok(channel) => channel,
            Err(error) => {
                let _ = self.service.fail_reserved(
                    &reservation,
                    &error.to_string(),
                    TurnErrorCode::InternalError,
                );
                return Err(CliError::Client(error));
            }
        };
        let driver = self.service.driver();
        let run = driver.run_observed(&reservation, observer);
        tokio::pin!(run);
        let mut poll = tokio::time::interval(CALLBACK_POLL);
        poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut interrupted = false;
        let outcome = loop {
            tokio::select! {
                biased;
                outcome = &mut run => break outcome,
                () = self.interrupt.recv(), if !interrupted => {
                    interrupted = true;
                    let _ = self.service.interrupt(&reservation.session_id, &reservation.turn_id);
                    break tokio::time::timeout(INTERRUPT_GRACE, &mut run)
                        .await
                        .unwrap_or(Err(DriverError::StaleTurn(reservation.turn_id.clone())));
                }
                update = updates.recv() => {
                    if update.is_some() {
                        output.consume(&self.history()?, stdout)?;
                    }
                }
                _ = poll.tick() => {
                    self.refuse_callbacks()?;
                    output.consume(&self.history()?, stdout)?;
                }
            }
        };
        while updates.try_recv().is_ok() {}
        let settled = match outcome {
            Ok(outcome) if !interrupted => {
                let turn = self.service.finish_reserved(&reservation, outcome)?;
                self.session_id.clone_from(&turn.session_id);
                Settled::Finished(turn.stop_reason)
            }
            Ok(_) => Settled::Interrupted,
            Err(_) if interrupted => Settled::Interrupted,
            Err(error) => {
                let published = public_driver_error(&error);
                let message = published.message.clone();
                let _ = self.service.fail_reserved_with(&reservation, published);
                output.consume(&self.history()?, stdout)?;
                return Err(CliError::TurnFailed(message));
            }
        };
        output.consume(&self.history()?, stdout)?;
        Ok(settled)
    }

    /// Reference `AppServerSession.deny_callback`: an approval is denied
    /// without feedback, and a question is answered as cancelled.
    fn refuse_callbacks(&mut self) -> Result<(), CliError> {
        for callback in self.service.drain_callbacks()? {
            let PublicHistoryEntry::Callback {
                callback_id,
                detail,
                state: PublicCallbackState::Open,
                ..
            } = callback
            else {
                continue;
            };
            let refusal = match detail {
                CallbackDetail::Approval { .. } => json!({
                    "type": "approval",
                    "decision": {"type": "deny"},
                    "feedback": null,
                }),
                CallbackDetail::UserInput { .. } => json!({
                    "type": "user_input",
                    "result": {"answers": [], "cancelled": true},
                }),
            };
            self.service.respond_callback(json!({
                "sessionId": self.session_id,
                "callbackId": callback_id,
                "output": refusal,
            }))?;
        }
        Ok(())
    }

    /// The session's history as the server holds it, every turn included.
    fn history(&mut self) -> Result<Vec<PublicHistoryEntry>, CliError> {
        Ok(self
            .service
            .session(&self.session_id)?
            .snapshot
            .map(|snapshot| snapshot.history)
            .unwrap_or_default())
    }
}

/// How a Teleport launch that never started is reported. Reference
/// `_teleport` raises `ProgrammaticTeleportError` when the repository names
/// no project, and lets every refusal of `open_projects` reach the launch as
/// the app-server error it is.
fn teleport_refusal(error: &ClientError) -> CliError {
    match error {
        ClientError::InvalidResponse(message) if message.contains("no Vibe Code project") => {
            CliError::Teleport("No Vibe Code project is linked to this repository".to_owned())
        }
        other => CliError::Session(client_error_message(other)),
    }
}

/// Reference `_require_teleport_available` (`vibe/app_server/_vibe_code.py`),
/// which `open_projects` runs before it reads the repository: Teleport needs
/// the active model on a Mistral provider and a key the console recognizes as
/// one Teleport accepts. The sentences are this port's own.
async fn require_teleport_available(workspace: &WorkspaceService) -> Result<(), CliError> {
    let on_mistral = workspace
        .layered_config()
        .load()
        .ok()
        .and_then(|snapshot| snapshot.active_provider())
        .is_some_and(|provider| {
            provider
                .get("backend")
                .and_then(toml::Value::as_str)
                .unwrap_or("mistral")
                == "mistral"
        });
    if !on_mistral {
        return Err(CliError::Session(
            "Teleport runs only on a Mistral model; switch to one with /model, then retry."
                .to_owned(),
        ));
    }
    let account = workspace.read_account().await;
    if account
        .get("teleportEligible")
        .and_then(serde_json::Value::as_bool)
        == Some(true)
    {
        return Ok(());
    }
    let action = account.get("teleportAction");
    let url = action
        .and_then(|action| action.get("url"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let switch_key = action
        .and_then(|action| action.get("kind"))
        .and_then(serde_json::Value::as_str)
        == Some("switch_api_key");
    Err(CliError::Session(if switch_key {
        format!("Teleport does not accept a Codestral key; use a Vibe or workspace key from {url}")
    } else {
        format!("Teleport could not verify your Mistral API key; check your sign-in at {url}")
    }))
}

/// Reference `_last_assistant_text`: the text of the last assistant message
/// that has any, across the whole session.
fn last_assistant_text(history: &[PublicHistoryEntry]) -> Option<String> {
    history.iter().rev().find_map(|entry| match entry {
        PublicHistoryEntry::Message {
            role: PublicMessageRole::Assistant,
            content,
            ..
        } => {
            let text = content
                .iter()
                .filter_map(|block| match block {
                    PublicContentBlock::Text { text } => Some(text.as_str()),
                    PublicContentBlock::Image { .. } | PublicContentBlock::Resource { .. } => None,
                })
                .collect::<Vec<_>>()
                .join("\n\n");
            (!text.is_empty()).then_some(text)
        }
        _ => None,
    })
}

/// Reference `ProgrammaticOutput`.
struct Output {
    mode: OutputMode,
    emitted: HashSet<String>,
    teleport_url: Option<String>,
}

impl Output {
    fn new(mode: OutputMode) -> Self {
        Self {
            mode,
            emitted: HashSet::new(),
            teleport_url: None,
        }
    }

    /// The history the session opened with, which a resumed session already
    /// holds and a stream starts from.
    fn start(
        &mut self,
        history: &[PublicHistoryEntry],
        stdout: &mut impl Write,
    ) -> Result<(), CliError> {
        self.consume(history, stdout)
    }

    /// Streams every entry that completed since the last reading, once.
    fn consume(
        &mut self,
        history: &[PublicHistoryEntry],
        stdout: &mut impl Write,
    ) -> Result<(), CliError> {
        if self.mode != OutputMode::Streaming {
            return Ok(());
        }
        for entry in history {
            if !entry.is_completed() || self.emitted.contains(&entry.metadata().id) {
                continue;
            }
            self.emitted.insert(entry.metadata().id.clone());
            let line = entry_document(entry)?.compact();
            writeln!(stdout, "{line}").map_err(CliError::Stdout)?;
            stdout.flush().map_err(CliError::Stdout)?;
        }
        Ok(())
    }

    fn teleport(
        &mut self,
        event: &ProgrammaticTeleportEvent,
        stdout: &mut impl Write,
    ) -> Result<(), CliError> {
        if let ProgrammaticTeleportEvent::Complete { url, .. } = event {
            self.teleport_url = Some(url.clone());
        }
        let progress = match self.mode {
            OutputMode::Streaming => {
                let line = Ordered::of(event).map_err(CliError::Json)?.compact();
                writeln!(stdout, "{line}").map_err(CliError::Stdout)?;
                return stdout.flush().map_err(CliError::Stdout);
            }
            OutputMode::Json => return Ok(()),
            OutputMode::Text => match event {
                ProgrammaticTeleportEvent::SummarizingContext { .. } => {
                    "Summarizing context...".to_owned()
                }
                ProgrammaticTeleportEvent::CheckingGit { .. } => {
                    "Preparing workspace...".to_owned()
                }
                ProgrammaticTeleportEvent::PushRequired { unpushed_count, .. } => {
                    format!("Pushing {unpushed_count} commit(s)...")
                }
                ProgrammaticTeleportEvent::Pushing { .. } => "Syncing with remote...".to_owned(),
                ProgrammaticTeleportEvent::StartingWorkflow { .. } => "Teleporting...".to_owned(),
                ProgrammaticTeleportEvent::Complete { .. }
                | ProgrammaticTeleportEvent::Failed { .. } => return Ok(()),
            },
        };
        writeln!(stdout, "{progress}").map_err(CliError::Stdout)
    }

    /// Writes what the launch ends on, and hands back what `text` mode prints.
    fn finalize(
        &self,
        history: &[PublicHistoryEntry],
        stdout: &mut impl Write,
    ) -> Result<Option<String>, CliError> {
        match self.mode {
            OutputMode::Streaming => Ok(None),
            OutputMode::Json => {
                let entries = history
                    .iter()
                    .map(entry_document)
                    .collect::<Result<Vec<_>, _>>()?;
                let payload = match &self.teleport_url {
                    Some(url) => Ordered::Object(vec![
                        ("history".to_owned(), Ordered::Array(entries)),
                        ("teleportUrl".to_owned(), Ordered::String(url.clone())),
                    ]),
                    None => Ordered::Array(entries),
                };
                writeln!(stdout, "{}", payload.pretty()).map_err(CliError::Stdout)?;
                stdout.flush().map_err(CliError::Stdout)?;
                Ok(None)
            }
            OutputMode::Text => Ok(self
                .teleport_url
                .clone()
                .or_else(|| last_assistant_text(history))),
        }
    }
}

/// One history entry as the reference dumps it: its base fields, then the
/// discriminator, then the fields of its own type.
fn entry_document(entry: &PublicHistoryEntry) -> Result<Ordered, CliError> {
    let mut document = Ordered::of(entry).map_err(CliError::Json)?;
    document.move_after("type", "relatedEntryId");
    layout::arrange(&mut document);
    Ok(document)
}

#[cfg(test)]
mod programmatic_tests;
