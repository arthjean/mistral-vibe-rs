//! Reference `run_cli` update routes: the forced `--check-upgrade` check and the
//! cached startup prompt, both of which resolve before any session starts.

use std::io::Write;
use std::path::Path;

use vibe_app_server::release3::Release3Service;
use vibe_core::updates::{
    PyPiUpdateGateway, UpdateCacheStore, UpdateGateway, dismiss_update, get_update_if_available,
    pending_update_from_cache,
};

use crate::Arguments;

use super::super::updates::{
    CheckUpgradeOutcome, UpdatePromptMode, UpdatePromptResult, classify_check_upgrade,
    update_failed_message,
};
use super::{StartupError, dialog, vibe_home_directory};

/// The PyPI project the reference checks.
const UPDATE_PROJECT: &str = "mistral-vibe";

/// Overrides the release index, so an isolated run never reaches the network.
const UPDATE_BASE_URL_ENVIRONMENT: &str = "VIBE_UPDATE_BASE_URL";

fn gateway_base_url() -> String {
    std::env::var(UPDATE_BASE_URL_ENVIRONMENT)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "https://pypi.org".to_owned())
}

#[must_use]
pub fn update_cache_store(arguments: &Arguments, working_directory: &Path) -> UpdateCacheStore {
    UpdateCacheStore::new(&vibe_home_directory(arguments, working_directory))
}

/// Reference `_run_check_upgrade`: report the result and exit without a session.
///
/// Returns `true` when the reference exits non-zero.
pub async fn run_check_upgrade(
    arguments: &Arguments,
    current_version: &str,
    output: &mut impl Write,
) -> Result<bool, StartupError> {
    let working_directory =
        std::env::current_dir().map_err(|error| super::startup_io(Path::new("."), error))?;
    let store = update_cache_store(arguments, &working_directory);
    let gateway = match PyPiUpdateGateway::with_base_url(UPDATE_PROJECT, gateway_base_url()) {
        Ok(gateway) => gateway,
        Err(error) => {
            report(
                output,
                &super::super::updates::check_failed_message(&error.user_message()),
            )?;
            return Ok(true);
        }
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
            let choice = dialog::run_update_dialog(
                current_version,
                &latest_version,
                UpdatePromptMode::CheckUpgrade,
            )?;
            match choice {
                // The reference dismisses only on the startup route.
                UpdatePromptResult::Continue | UpdatePromptResult::Quit => Ok(false),
                UpdatePromptResult::UpdateUnavailable => {
                    report(output, &update_failed_message(current_version))?;
                    Ok(true)
                }
            }
        }
    }
}

/// Reference `_maybe_run_startup_update_prompt`: a cached, dismissible offer that
/// never contacts the network and never starts a session on quit.
///
/// Returns `false` when startup must stop.
pub fn resolve_startup_update_prompt(
    arguments: &Arguments,
    working_directory: &Path,
    release3: &Release3Service,
    current_version: &str,
    output: &mut impl Write,
) -> Result<bool, StartupError> {
    if !update_checks_enabled(release3) {
        return Ok(true);
    }
    let store = update_cache_store(arguments, working_directory);
    let cache = store.load();
    let Some(latest_version) = pending_update_from_cache(cache.as_ref(), current_version) else {
        return Ok(true);
    };
    match dialog::run_update_dialog(current_version, &latest_version, UpdatePromptMode::Startup)? {
        UpdatePromptResult::Continue => {
            if let Some(dismissed) = dismiss_update(cache.as_ref(), &latest_version) {
                // The reference logs and continues when dismissal cannot persist.
                let _ = store.store(&dismissed);
            }
            Ok(true)
        }
        UpdatePromptResult::Quit => Ok(false),
        UpdatePromptResult::UpdateUnavailable => {
            report(output, &update_failed_message(current_version))?;
            Ok(false)
        }
    }
}

/// Reference `config.enable_update_checks`, defaulting to the schema default when
/// the configuration cannot be read.
#[must_use]
pub fn update_checks_enabled(release3: &Release3Service) -> bool {
    release3
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
/// cache for the next startup and never renders anything itself.
pub async fn refresh_update_cache(
    gateway: &dyn UpdateGateway,
    store: &UpdateCacheStore,
    current_version: &str,
) {
    let _ = get_update_if_available(
        gateway,
        store,
        current_version,
        vibe_core::clock::now_seconds_signed(),
        false,
    )
    .await;
}

#[must_use]
pub fn production_update_gateway() -> Option<PyPiUpdateGateway> {
    PyPiUpdateGateway::with_base_url(UPDATE_PROJECT, gateway_base_url()).ok()
}

fn report(output: &mut impl Write, message: &str) -> Result<(), StartupError> {
    writeln!(output, "{message}").map_err(|error| StartupError::Terminal(error.to_string()))
}
