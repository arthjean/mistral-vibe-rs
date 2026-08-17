//! The `vibe-acp` binary: what one editor session is started with, before the
//! stdio transport takes over.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod stdio;

use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;

use tokio::io::BufReader;
use vibe_acp::{ProductionAuthEnvironment, default_vibe_home};
use vibe_app_server::client::{LiveDriverConfig, LiveTurnDriver};
use vibe_app_server::experiments::Credentials;
use vibe_app_server::release3::{Release3Paths, Release3Service};
use vibe_core::auth::KeyringStore;
use vibe_core::compaction::manager::CompactionPromptResolution;
use vibe_core::config::DotenvValues;
use vibe_core::observability::{LogLevel, init_file_logging, log};
use vibe_core::telemetry::{
    ExperimentExposures, LaunchContext, ReqwestTelemetryTransport, TelemetryClient,
    TelemetryConfig, TelemetryConfigGetter, TelemetryContext, TelemetryEventObserver,
};
use vibe_core::tracing::{TracingGuard, TracingSetup, setup_tracing};

use crate::stdio::driver::DeferredTurnDriver;
use crate::stdio::{StdioOptions, run_stdio};

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
    // One client carries the editor session's events: the turn's own, through
    // the driver, and the ones an editor records, through the app server.
    // The exposures are created before the client so a rollout resolved by any
    // session of this process reaches the census of every event it sends.
    let exposures = ExperimentExposures::default();
    let telemetry = acp_telemetry_observer(&vibe_home, &dotenv, exposures.clone());
    let experiments = telemetry.as_ref().map(|_| vibe_acp::AcpExperiments {
        exposures,
        credentials: acp_credentials(&vibe_home),
        launch: acp_launch_context(),
    });
    let driver = DeferredTurnDriver::new({
        let vibe_home = vibe_home.clone();
        let telemetry = telemetry.clone();
        move || {
            let driver = LiveTurnDriver::from_environment(
                config.clone(),
                &DotenvValues::global(&vibe_home),
            )?;
            Ok(match telemetry.clone() {
                Some(telemetry) => driver.with_event_observer(telemetry),
                None => driver,
            })
        }
    });
    run_stdio(
        BufReader::new(tokio::io::stdin()),
        tokio::io::stdout(),
        driver,
        StdioOptions {
            session_root: Some(session_root),
            credential_environment,
            auth_environment: Arc::new(ProductionAuthEnvironment::new(vibe_home)),
            production_cloud: true,
            telemetry,
            experiments,
        },
    )
    .await
}

/// How a variable becomes a credential for this process: the same resolution
/// every provider read goes through, as the handle a detached lookup carries.
fn acp_credentials(vibe_home: &Path) -> Credentials {
    let environment = DotenvValues::global(vibe_home).environment();
    let store = KeyringStore::native();
    Arc::new(move |name: &str| vibe_core::auth::resolve_api_key(name, &environment, &store))
}

/// What this binary reports about itself on every event. Reference
/// `build_launch_context` at [acp/entrypoint.py:82-87], whose entrypoint is
/// `acp` and whose client is `vibe_acp` with no terminal emulator, since an
/// editor session is attached to none.
fn acp_launch_context() -> LaunchContext {
    LaunchContext {
        agent_entrypoint: "acp".to_owned(),
        agent_version: env!("CARGO_PKG_VERSION").to_owned(),
        client_name: "vibe_acp".to_owned(),
        client_version: env!("CARGO_PKG_VERSION").to_owned(),
        terminal_emulator: None,
    }
}

/// The configuration this process reads before any session exists: the merged
/// document under `vibe_home`, resolved against the directory the editor
/// launched the adapter from. Both the telemetry client and the span exporter
/// read the same one.
fn ambient_release3(vibe_home: &Path) -> Option<Release3Service> {
    Release3Service::new(
        Release3Paths {
            vibe_home: vibe_home.to_path_buf(),
            working_directory: std::env::current_dir().unwrap_or_else(|_| vibe_home.to_path_buf()),
            session_root: vibe_home.join("sessions"),
        },
        false,
    )
    .ok()
}

fn acp_telemetry_context(exposures: ExperimentExposures) -> TelemetryContext {
    TelemetryContext {
        launch: Some(acp_launch_context()),
        experiments: exposures,
        ..TelemetryContext::default()
    }
}

/// The client every event of this editor session travels through.
///
/// Nothing here decides whether telemetry is on: [`TelemetryConfig::resolve`]
/// re-reads `enable_telemetry` and the Mistral provider from the merged
/// configuration on every send, which is the same key the reference reads on
/// this path (`vibe/acp/entrypoint.py:99-100`). A home that publishes no
/// configuration, or a transport that cannot be built, installs no client and
/// the editor session runs without telemetry.
fn acp_telemetry_observer(
    vibe_home: &Path,
    dotenv: &DotenvValues,
    exposures: ExperimentExposures,
) -> Option<Arc<TelemetryEventObserver<ReqwestTelemetryTransport>>> {
    let configuration = ambient_release3(vibe_home)?.layered_config();
    let environment = dotenv.environment();
    let store = KeyringStore::native();
    // Reference `resolve_api_key`: the environment the dotenv load left behind
    // first, then the OS keyring, which is where onboarding puts the key.
    let credentials =
        move |name: &str| vibe_core::auth::resolve_api_key(name, &environment, &store);
    let config: TelemetryConfigGetter = Arc::new(move || match configuration.load() {
        Ok(snapshot) => TelemetryConfig::resolve(&snapshot.effective, &credentials),
        Err(_) => TelemetryConfig::disabled(),
    });
    let transport = ReqwestTelemetryTransport::try_new().ok()?;
    Some(Arc::new(TelemetryEventObserver::new(
        TelemetryClient::new(config, transport),
        acp_telemetry_context(exposures),
    )))
}

/// Installs the span exporter this editor session exports through, when the
/// merged configuration asks for one. Reference reads `enable_telemetry` on the
/// ACP path too, and this port reads the same document the terminal client
/// reads.
fn install_tracing(vibe_home: &Path, dotenv: &DotenvValues) -> Option<TracingGuard> {
    let snapshot = ambient_release3(vibe_home)?.layered_config().load().ok()?;
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
