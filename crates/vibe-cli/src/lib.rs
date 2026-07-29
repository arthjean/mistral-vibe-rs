#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use std::io::Write;
use std::path::PathBuf;

use clap::{ArgAction, Parser, ValueEnum};
use serde::Serialize;
use thiserror::Error;
use vibe_app_server::client::{
    ClientError, HeadlessService, LiveDriverConfig, LiveTurnDriver, ProgrammaticTeleportEvent,
    ProgrammaticTurn, ProgrammaticUpdate, PublicTurnStopReason, SessionOptions, TurnDriver,
    programmatic_update_channel,
};

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
    pub version: bool,
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
        let driver = LiveTurnDriver::from_environment(LiveDriverConfig {
            style: arguments.provider_style.clone(),
            endpoint: arguments.api_base.clone(),
            model: arguments.model.clone(),
            credential_environment: arguments.credential_environment.clone(),
            system_prompt: "You are Mistral Vibe.".to_owned(),
            session_root: arguments.session_root.clone(),
            input_price_per_million_micros: price_per_million_micros(arguments.input_price)?,
            output_price_per_million_micros: price_per_million_micros(arguments.output_price)?,
        })?;
        execute(arguments, driver, stdout, stderr).await
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
        max_turns: arguments.max_turns,
        max_tokens: arguments.max_tokens,
        max_price_micros,
        auto_approve: arguments.auto_approve,
        resume: arguments.resume.clone(),
        continue_session: arguments.continue_session,
    };
    let mut service = HeadlessService::new(driver)?;
    let session_id = service.start_session(&options)?;
    let mut close_session_id = session_id.clone();
    let turn_result: Result<ProgrammaticTurn, CliError> =
        if arguments.output == OutputMode::Streaming {
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
                while let Ok(ProgrammaticUpdate::HistoryEntry { entry, .. }) = updates.try_recv() {
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
                write_teleport_events(arguments.output, &turn.teleport_events, stdout)?;
            match turn.stop_reason {
                PublicTurnStopReason::Complete => {
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
                PublicTurnStopReason::MaxSteps
                | PublicTurnStopReason::TokenLimit
                | PublicTurnStopReason::PriceLimit => {
                    Err(CliError::Limit(if !turn.final_assistant.is_empty() {
                        turn.final_assistant
                    } else {
                        "The configured conversation limit was reached".to_owned()
                    }))
                }
                PublicTurnStopReason::ResponseLength => Err(CliError::TurnFailed(
                    "The model's response exceeded the maximum output token limit.".to_owned(),
                )),
                PublicTurnStopReason::Refusal => Err(CliError::TurnFailed(
                    "Provider refused the request".to_owned(),
                )),
                PublicTurnStopReason::Cancelled => {
                    Err(CliError::TurnFailed("Turn cancelled".to_owned()))
                }
                PublicTurnStopReason::Failed => Err(CliError::TurnFailed("Turn failed".to_owned())),
            }
        }
        Err(error) => Err(error),
    };
    let close_result = service.close_session(&close_session_id);
    let shutdown_result = service.shutdown();
    execution?;
    close_result?;
    shutdown_result?;
    Ok(())
}

fn validate_arguments(arguments: &Arguments) -> Result<(), CliError> {
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
    #[error(transparent)]
    Json(serde_json::Error),
    #[error(transparent)]
    Client(#[from] ClientError),
    #[error(transparent)]
    Driver(#[from] vibe_app_server::client::DriverError),
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
    use vibe_app_server::client::EchoTurnDriver;

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
            version: false,
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
