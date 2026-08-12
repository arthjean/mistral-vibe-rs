//! The first-run onboarding flow: the reference's screen graph in place of a
//! chat-transcript questionnaire.
//!
//! Reference `vibe/setup/onboarding/`. The flow is reached exactly the two
//! ways the reference reaches it, `--setup` and an interactive launch with no
//! resolvable credential, and terminates with one of five values whose exit
//! paths [`exit_plan`] maps: a cancellation leaves nothing behind and exits
//! cleanly, an unusable key variable exits with a failure, and every other
//! path persists the chosen theme once, after the screens have closed.

pub mod context;
mod driver;
pub mod model;
mod render;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use toml::{Table, Value};
use vibe_app_server::release3::Release3Service;
use vibe_core::auth::PersistOutcome;
use vibe_core::telemetry::TelemetryRecord;

use self::context::OnboardingContext;
use self::model::{OnboardingOutcome, OnboardingPorts};
use super::setup::PersistedCredentialStore;
use super::startup;
use crate::{Arguments, CliError, CliTelemetryObserver};

/// How the caller leaves the flow. Reference `run_onboarding` callers:
/// `--setup` exits after any conclusion, and the interactive launch continues
/// into the session only when the flow did not exit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OnboardingConclusion {
    /// The process ends with this code, as a cancellation and an unusable key
    /// variable do in the reference.
    Exit(u8),
    /// The flow finished; the session may start.
    Continue,
}

/// What one terminating value does on the way out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExitPlan {
    /// `Some` ends the process; `None` lets the caller continue.
    pub exit_code: Option<u8>,
    /// The operator-facing conclusion, printed after the terminal restores.
    pub message: String,
    /// Whether the chosen theme is written to the configuration, which every
    /// path but a cancellation and an unusable key variable does.
    pub persist_theme: bool,
}

/// Reference `run_onboarding`'s match over the five terminating values. The
/// sentences are this port's own; `NOTICE` keeps them permanently unequal to
/// the reference's.
#[must_use]
pub fn exit_plan(outcome: &OnboardingOutcome, global_env_file: &Path) -> ExitPlan {
    match outcome {
        OnboardingOutcome::Cancelled => ExitPlan {
            exit_code: Some(0),
            message: "Setup canceled. Nothing was saved; run vibe again whenever you are ready."
                .to_owned(),
            persist_theme: false,
        },
        OnboardingOutcome::EnvVarError { detail } => ExitPlan {
            exit_code: Some(1),
            message: format!(
                "The API key was not saved: this provider names an unusable environment \
                 variable ({detail}). Update the provider's api_key_env_var setting in your \
                 configuration and run setup again."
            ),
            persist_theme: false,
        },
        OnboardingOutcome::SaveError { detail } => ExitPlan {
            exit_code: None,
            message: format!(
                "Warning: the API key could not be stored durably: {detail}. The key stays \
                 available for this session only; you may need to add it to {} yourself.",
                global_env_file.display()
            ),
            persist_theme: true,
        },
        OnboardingOutcome::ProviderConfigError { detail } => ExitPlan {
            exit_code: None,
            message: format!(
                "Warning: the API key was saved, but the provider entry was not: {detail}. \
                 Update the provider's browser_auth_base_url and browser_auth_api_base_url \
                 settings in config.toml yourself."
            ),
            persist_theme: true,
        },
        OnboardingOutcome::Completed => ExitPlan {
            exit_code: None,
            message: "Setup is complete. Run vibe to start your first session.".to_owned(),
            persist_theme: true,
        },
    }
}

/// The production ports: the shared credential store for the key, and the
/// configuration service for the provider entry.
struct ProductionPorts<'a> {
    store: &'a PersistedCredentialStore,
    release3: &'a Release3Service,
    /// Where the onboarding event goes. Absent when the transport could not be
    /// built, which leaves onboarding working and silent.
    telemetry: Option<Arc<CliTelemetryObserver>>,
}

impl OnboardingPorts for ProductionPorts<'_> {
    fn persist_api_key(
        &mut self,
        env_key: &str,
        provider: &Table,
        api_key: &str,
        custom_domain: bool,
    ) -> PersistOutcome {
        let backend_is_mistral = provider.get("backend").and_then(Value::as_str) == Some("mistral");
        let report = self
            .store
            .persist_report(env_key, backend_is_mistral, api_key, custom_domain);
        // Reference `send_onboarding_api_key_added`, raised for a Mistral
        // provider on every completed save. No session exists yet, so the
        // census reports none.
        if let Some(telemetry) = self.telemetry.as_ref() {
            for event in &report.telemetry {
                let _ = telemetry.enqueue(
                    &TelemetryRecord::OnboardingApiKeyAdded {
                        custom_domain: event.custom_domain,
                    },
                    None,
                );
            }
        }
        report.outcome
    }

    fn persist_provider(&mut self, provider: &Table) -> bool {
        self.release3.persist_provider(provider).is_ok()
    }
}

/// Runs the onboarding flow end to end: loads the context from the effective
/// configuration, drives the screens, then takes the exit path the
/// terminating value maps to, persisting the theme once on the paths that
/// keep it.
pub async fn run_onboarding(
    arguments: &Arguments,
    working_directory: &Path,
    vibe_home: &Path,
    credential_store: &PersistedCredentialStore,
) -> Result<OnboardingConclusion, CliError> {
    let release3 = startup::startup_host(arguments, working_directory)
        .into_release3(arguments.trust)
        .map_err(startup::StartupError::from)?;
    // Reference `OnboardingContext.load` falls back to the shipped defaults
    // rather than failing when the configuration cannot be read.
    let document = release3.config_document().ok();
    let context = OnboardingContext::from_effective_config(
        document
            .as_ref()
            .and_then(|document| document.get("config")),
    );
    let telemetry = crate::telemetry_observer(arguments, &release3).ok();
    let mut ports = ProductionPorts {
        store: credential_store,
        release3: &release3,
        telemetry: telemetry.clone(),
    };
    let run = driver::run_flow(context, &mut ports).await?;
    if let Some(telemetry) = telemetry {
        telemetry.flush().await;
    }
    let plan = exit_plan(&run.outcome, &global_env_file(vibe_home));
    if plan.persist_theme {
        persist_theme(&release3, run.selected_theme);
    }
    println!("\n{}\n", plan.message);
    Ok(match plan.exit_code {
        Some(code) => OnboardingConclusion::Exit(code),
        None => OnboardingConclusion::Continue,
    })
}

fn global_env_file(vibe_home: &Path) -> PathBuf {
    vibe_core::config::global_env_file(vibe_home)
}

/// Writes the chosen theme once, after the screens have closed. Reference
/// `run_onboarding` sets `/theme` through the orchestrator with the
/// onboarding reason; a write that fails is reported, not fatal.
fn persist_theme(release3: &Release3Service, theme: &str) {
    let params = std::collections::BTreeMap::from([(
        "writes".to_owned(),
        serde_json::json!([{
            "target": "user",
            "mutations": [{"path": ["theme"], "value": theme}],
        }]),
    )]);
    if let Err(error) = release3.dispatch("config/batchWrite", &params) {
        eprintln!("Warning: the chosen theme could not be saved: {error}");
    }
}

#[cfg(test)]
mod context_tests;
#[cfg(test)]
mod model_tests;
