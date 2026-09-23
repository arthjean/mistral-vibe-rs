//! Reference `run_cli` update routes: the forced `--check-upgrade` check and the
//! cached startup prompt, both of which resolve before any session starts.

use std::io::Write;
use std::path::Path;

use vibe_app_server::workspace::WorkspaceService;
use vibe_core::observability::{self, LogLevel};
use vibe_core::updates::{
    GitHubUpdateGateway, UpdateCacheStore, UpdateGateway, UpdateGatewayCause, dismiss_update,
    get_update_if_available, pending_update_from_cache, run_upgrade_commands,
};

use crate::Arguments;
use crate::distribution::{REPOSITORY_URL, upgrade_commands};

use super::super::updates::{
    CheckUpgradeOutcome, UPDATING_MESSAGE, UpdateChoice, UpdatePromptMode, UpdatePromptResult,
    check_failed_message, classify_check_upgrade, update_failed_message, updated_message,
};
use super::{StartupError, dialog, vibe_home_directory};

#[cfg(test)]
mod gateway_tests;

/// Overrides the release index, so an isolated run never reaches the network.
const UPDATE_BASE_URL_ENVIRONMENT: &str = "VIBE_UPDATE_BASE_URL";

/// The token the reference sends when one is exported.
const GITHUB_TOKEN_ENVIRONMENT: &str = "GITHUB_TOKEN";

fn gateway_base_url() -> String {
    std::env::var(UPDATE_BASE_URL_ENVIRONMENT)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| GitHubUpdateGateway::DEFAULT_BASE_URL.to_owned())
}

/// The owner and repository of the distribution this binary was installed
/// from, read from the manifest rather than written out here.
#[must_use]
pub fn release_repository() -> Option<(&'static str, &'static str)> {
    REPOSITORY_URL
        .strip_prefix("https://github.com/")?
        .trim_end_matches('/')
        .trim_end_matches(".git")
        .split_once('/')
        .filter(|(owner, repository)| !owner.is_empty() && !repository.is_empty())
}

#[must_use]
pub fn update_cache_store(arguments: &Arguments, working_directory: &Path) -> UpdateCacheStore {
    UpdateCacheStore::new(&vibe_home_directory(arguments, working_directory))
}

/// Reference `_run_check_upgrade`: report the result and exit without a session.
///
/// The launch reaches this with `--workdir` and `--add-dir` already validated,
/// and it loads the configuration before it asks anything, as `run_cli` calls
/// `load_config_orchestrator_or_exit` first: a configuration no session could
/// start from fails the check too, and the configured theme paints the prompt.
///
/// Returns `true` when the reference exits non-zero.
pub async fn run_check_upgrade(
    arguments: &Arguments,
    current_version: &str,
    output: &mut impl Write,
) -> Result<bool, StartupError> {
    let working_directory = match &arguments.workdir {
        Some(directory) => directory.clone(),
        None => {
            std::env::current_dir().map_err(|error| super::startup_io(Path::new("."), error))?
        }
    };
    let theme = match configured_theme(arguments, &working_directory) {
        Ok(theme) => theme,
        Err(message) => {
            report(output, &message)?;
            return Ok(true);
        }
    };
    let store = update_cache_store(arguments, &working_directory);
    let Some(gateway) = production_update_gateway() else {
        // Unreachable while the manifest declares a GitHub repository, which
        // `the_update_gateway_names_the_repository_the_manifest_declares` pins.
        report(
            output,
            &check_failed_message(UpdateGatewayCause::Unknown.default_message()),
        )?;
        return Ok(true);
    };
    let result = get_update_if_available(
        &gateway,
        &store,
        current_version,
        vibe_core::clock::now_seconds_signed(),
        true,
    )
    .await;
    match classify_check_upgrade(result, current_version) {
        CheckUpgradeOutcome::UpToDate { message } => {
            report(output, &message)?;
            Ok(false)
        }
        CheckUpgradeOutcome::Failed { message } => {
            report(output, &message)?;
            Ok(true)
        }
        CheckUpgradeOutcome::Prompt { latest_version } => {
            let choice = resolve_update_prompt(
                current_version,
                &latest_version,
                UpdatePromptMode::CheckUpgrade,
                theme.as_deref(),
                output,
            )
            .await?;
            match choice {
                // The reference dismisses only on the startup route.
                UpdatePromptResult::Continue | UpdatePromptResult::Quit => Ok(false),
                UpdatePromptResult::Updated => {
                    report(output, &updated_message(current_version, &latest_version))?;
                    Ok(false)
                }
                UpdatePromptResult::UpdateFailed => {
                    report(output, &update_failed_message(current_version))?;
                    Ok(true)
                }
            }
        }
    }
}

/// Reference `load_config_orchestrator_or_exit` as `--check-upgrade` runs it:
/// the workspace the launch would open, its configuration brought forward and
/// composed, and the theme it names. The project half counts only where the
/// workspace is already trusted, because nothing on this route may ask.
///
/// The error is the sentence the launch prints before it exits 1.
fn configured_theme(
    arguments: &Arguments,
    working_directory: &Path,
) -> Result<Option<String>, String> {
    let host = super::startup_host(arguments, working_directory);
    let trusted = arguments.trust
        || host
            .inspect_workspace_trust()
            .is_ok_and(|inspection| inspection.trusted);
    let document = host
        .into_workspace(trusted)
        .map_err(|error| error.to_string())?
        .config_document()
        .map_err(|error| error.to_string())?;
    Ok(document
        .get("config")
        .and_then(|config| config.get("theme"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned))
}

/// Reference `_maybe_run_startup_update_prompt`: a cached, dismissible offer that
/// never contacts the network and never starts a session on quit.
///
/// `store` is the one the whole launch shares, as the reference hands one
/// repository from `run_cli` to the app.
///
/// Returns the process exit code the prompt decided, or `None` when startup
/// continues into a session.
pub async fn resolve_startup_update_prompt(
    workspace: &WorkspaceService,
    store: &UpdateCacheStore,
    current_version: &str,
    output: &mut impl Write,
) -> Result<Option<u8>, StartupError> {
    if !update_checks_enabled(workspace) {
        return Ok(None);
    }
    let cache = store.load();
    let Some(latest_version) = pending_update_from_cache(cache.as_ref(), current_version) else {
        return Ok(None);
    };
    let theme = workspace.config_document().ok().and_then(|document| {
        document
            .get("config")?
            .get("theme")?
            .as_str()
            .map(str::to_owned)
    });
    let result = resolve_update_prompt(
        current_version,
        &latest_version,
        UpdatePromptMode::Startup,
        theme.as_deref(),
        output,
    )
    .await?;
    match result {
        UpdatePromptResult::Continue => {
            if let Some(dismissed) = dismiss_update(cache.as_ref(), &latest_version) {
                store.store(&dismissed);
            }
        }
        UpdatePromptResult::Updated => {
            report(output, &updated_message(current_version, &latest_version))?;
        }
        UpdatePromptResult::UpdateFailed => {
            report(output, &update_failed_message(current_version))?;
        }
        UpdatePromptResult::Quit => {}
    }
    Ok(result.exit_code())
}

/// Reference `UpdatePromptDialog`: the operator answers, and choosing `Update
/// now` runs the upgrade before the prompt reports its result.
async fn resolve_update_prompt(
    current_version: &str,
    latest_version: &str,
    mode: UpdatePromptMode,
    theme: Option<&str>,
    output: &mut impl Write,
) -> Result<UpdatePromptResult, StartupError> {
    match dialog::run_update_dialog(current_version, latest_version, mode, theme)? {
        None => Ok(UpdatePromptResult::Quit),
        Some(UpdateChoice::Continue) => Ok(UpdatePromptResult::Continue),
        Some(UpdateChoice::Update) => {
            report(output, UPDATING_MESSAGE)?;
            Ok(run_upgrade(&upgrade_commands()).await)
        }
    }
}

/// Reference `do_update` as the dialog observes it. Ctrl+C is this port's
/// cancellation: the terminal is already restored when the commands run, so the
/// signal is what the reference's cancelled task answers to.
async fn run_upgrade(commands: &[String]) -> UpdatePromptResult {
    let cancelled = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    UpdatePromptResult::from_upgrade(run_upgrade_commands(commands, cancelled).await)
}

/// Reference `config.enable_update_checks`, defaulting to the schema default when
/// the configuration cannot be read.
#[must_use]
pub fn update_checks_enabled(workspace: &WorkspaceService) -> bool {
    workspace
        .config_document()
        .ok()
        .and_then(|document| {
            document
                .get("config")?
                .get("enable_update_checks")?
                .as_bool()
        })
        .unwrap_or(true)
}

/// The background check the reference schedules after mount: it refreshes the
/// cache for the next startup and never renders anything itself. A failed check
/// is logged as a warning, as `_check_update` logs it.
pub async fn refresh_update_cache(
    gateway: &dyn UpdateGateway,
    store: &UpdateCacheStore,
    current_version: &str,
) {
    if let Err(error) = get_update_if_available(
        gateway,
        store,
        current_version,
        vibe_core::clock::now_seconds_signed(),
        false,
    )
    .await
    {
        observability::log(LogLevel::Warning, &format!("Update check failed\n{error}"));
    }
}

/// The gateway the running distribution is published through: the releases of
/// the repository the manifest declares, carrying the exported token when there
/// is one.
#[must_use]
pub fn production_update_gateway() -> Option<GitHubUpdateGateway> {
    let (owner, repository) = release_repository()?;
    Some(
        GitHubUpdateGateway::with_base_url(owner, repository, gateway_base_url())
            .ok()?
            .with_token(std::env::var(GITHUB_TOKEN_ENVIRONMENT).ok()),
    )
}

/// Reference `_schedule_update_notification`: a disabled check builds no
/// gateway, so nothing can send a request.
#[must_use]
pub fn scheduled_update_gateway(enabled: bool) -> Option<GitHubUpdateGateway> {
    enabled.then(production_update_gateway).flatten()
}

fn report(output: &mut impl Write, message: &str) -> Result<(), StartupError> {
    writeln!(output, "{message}").map_err(|error| StartupError::Terminal(error.to_string()))
}
