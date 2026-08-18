//! Everything an interactive launch settles before a session can exist.
//!
//! Trust, location safety, bare-resume selection, onboarding and the update
//! prompt each answer the same question: does this launch continue, and with
//! which arguments? Each of them may also refuse the launch outright, so the
//! whole preflight answers one [`ControlFlow`] rather than repeating the same
//! "nothing started" exit at every step.

use std::ops::ControlFlow;
use std::path::PathBuf;

use vibe_app_server::release3::Release3Service;

use super::{InteractiveInvocation, ResumeResolution, StartupError};
use crate::tui::onboarding::{self, OnboardingConclusion};
use crate::tui::setup::PersistedCredentialStore;
use crate::{Arguments, CliError, validate_arguments};

/// A launch that reached the point where a session may be opened.
pub(in crate::tui) struct ReadyStartup {
    pub(in crate::tui) arguments: Arguments,
    pub(in crate::tui) working_directory: PathBuf,
    pub(in crate::tui) release3: Release3Service,
    /// The credential the session starts under, or `None` when onboarding was
    /// declined and the client opens read-only.
    pub(in crate::tui) credential: Option<String>,
    pub(in crate::tui) post_mount_action: Option<super::PostMountAction>,
}

/// Runs every pre-session gate in reference order.
///
/// [`ControlFlow::Break`] carries the process exit code the refusing gate
/// decided, which is how a cancelled onboarding exits 0 and an unusable key
/// variable exits 1, as the reference's `run_onboarding` callers do.
pub(in crate::tui) async fn preflight(
    invocation: InteractiveInvocation,
) -> Result<ControlFlow<Option<u8>, ReadyStartup>, CliError> {
    let InteractiveInvocation {
        mut arguments,
        post_mount_action,
        ..
    } = invocation;
    validate_arguments(&arguments)?;
    let working_directory = match &arguments.workdir {
        Some(path) => path.clone(),
        None => std::env::current_dir().map_err(CliError::CurrentDirectory)?,
    };
    let startup_host = super::startup_host(&arguments, &working_directory);
    let trust = super::resolve_workspace_trust(&mut arguments, &startup_host)?;
    if trust.cancelled {
        return Ok(ControlFlow::Break(None));
    }
    if !super::resolve_location_safety(trust.dangerous_warning.as_deref())? {
        return Ok(ControlFlow::Break(None));
    }
    match super::resolve_bare_resume(&arguments, &startup_host)? {
        ResumeResolution::Unchanged => {}
        ResumeResolution::StartNew => arguments.resume = None,
        ResumeResolution::Resume(session_id) => {
            arguments.resume = Some(session_id);
            arguments.continue_session = false;
        }
        ResumeResolution::Abort => return Ok(ControlFlow::Break(None)),
    }
    let credential = match resolve_credential(&arguments, &working_directory).await? {
        ControlFlow::Break(exit_code) => return Ok(ControlFlow::Break(exit_code)),
        ControlFlow::Continue(credential) => credential,
    };
    let release3 = startup_host
        .into_release3(arguments.trust)
        .map_err(StartupError::from)?;
    if !super::resolve_startup_update_prompt(
        &arguments,
        &working_directory,
        &release3,
        env!("CARGO_PKG_VERSION"),
        &mut std::io::stdout().lock(),
    )? {
        return Ok(ControlFlow::Break(None));
    }
    Ok(ControlFlow::Continue(ReadyStartup {
        arguments,
        working_directory,
        release3,
        credential,
        post_mount_action,
    }))
}

/// Resolves the API credential, running the onboarding screens when the launch
/// asked for them or when nothing else answers.
///
/// Reference `run_cli` and `load_config_orchestrator_or_exit`: `--setup` always
/// runs the onboarding screens and exits afterward, and an interactive launch
/// with no resolvable credential runs them and then continues into the session
/// it can now start.
async fn resolve_credential(
    arguments: &Arguments,
    working_directory: &std::path::Path,
) -> Result<ControlFlow<Option<u8>, Option<String>>, CliError> {
    let vibe_home = super::vibe_home_directory(arguments, working_directory);
    let store = PersistedCredentialStore::new(vibe_core::config::global_env_file(&vibe_home));
    // The process environment first, then `{vibe_home}/.env`, then the keyring
    // under the shared service names: a key the operator keeps in the dotenv
    // file is as usable here as an exported one, and a keyring that cannot be
    // reached reads as absent, as the reference reads it.
    let resolve = || {
        vibe_core::config::DotenvValues::global(&vibe_home)
            .variable(&arguments.credential_environment)
            .filter(|credential| !credential.is_empty())
            .or_else(|| store.resolve(&arguments.credential_environment))
    };
    let credential = resolve();
    if !arguments.setup && credential.is_some() {
        return Ok(ControlFlow::Continue(credential));
    }
    match onboarding::run_onboarding(arguments, working_directory, &vibe_home, &store).await? {
        OnboardingConclusion::Exit(code) => Ok(ControlFlow::Break(Some(code))),
        OnboardingConclusion::Continue if arguments.setup => Ok(ControlFlow::Break(None)),
        OnboardingConclusion::Continue => Ok(ControlFlow::Continue(resolve())),
    }
}
