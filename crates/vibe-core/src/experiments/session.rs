//! What a session does with a rollout: when it is allowed to ask, what it
//! posts, what it keeps, and what it reads back on a resume.
//!
//! Two gates decide whether anything happens at all, and both are read off the
//! merged configuration rather than latched at startup: `enable_telemetry` and
//! `experiments.enable`. An operator who turned either off issues zero eval
//! requests and zero identity requests, which is the property the privacy
//! requirement rests on. Two more conditions follow from the reference's own
//! rule that a credential never leaves for an endpoint it does not belong to:
//! there has to be a Mistral provider, and its variable has to resolve.
//!
//! What the lookup posts about the caller is built here as well, and the one
//! attribute that could identify a person is not one: `userId` is
//! [`hash_api_key`] of the credential, so bucketing is stable per user without
//! the key leaving the process.
//!
//! Reference: `vibe/core/experiments/session.py` at the pinned commit.

use std::time::Duration;

use toml::Table;

use crate::config::registry::default_document;
use crate::identity::IdentityResolver;
use crate::telemetry::{LaunchContext, mistral_provider, platform_id};

use super::manager::{ExperimentManager, hash_api_key};
use super::models::{EvalResponse, ExperimentAttributes};

/// How long the identity request that fills `organizationId` is given.
///
/// Reference `EXPERIMENT_IDENTITY_TIMEOUT_S`, whose four seconds are held here
/// as the duration a request is issued with, so the number is spelled once.
/// Shorter than the eval budget, because the identity read is one step of a
/// lookup that already fails open: an organization that cannot be resolved
/// costs an attribute, not a session.
pub const EXPERIMENT_IDENTITY_TIMEOUT: Duration = Duration::from_millis(4_000);

/// How a variable becomes a credential.
///
/// `Send` and `Sync` because the lookup runs in a detached task: the reference
/// resolves its key on the same task, and this port's equivalent is a closure
/// that has to cross one.
pub type CredentialSource = dyn Fn(&str) -> Option<String> + Send + Sync;

/// How a resolved rollout reaches the session's own record of itself.
///
/// Reference `SessionLogger.persist_experiments`, whose failures are suppressed
/// at the call site; the same rule holds here, so an implementation reports
/// nothing and a session that cannot write its metadata still runs.
pub trait ExperimentStateSink: Send + Sync {
    fn persist(&self, state: &EvalResponse);
}

/// Looks the rollout up for this session and records what it resolved.
///
/// Answers whether anything changed, which is what decides a configuration
/// refresh: a gate that stopped the lookup, a missing credential and a failed
/// eval all answer `false`, because in every one of those cases the manager is
/// still on its declared defaults and refreshing would recompose the same
/// document.
///
/// Reference `initialize_experiments`.
pub async fn initialize_experiments(
    effective: &Table,
    credentials: &CredentialSource,
    manager: &mut ExperimentManager,
    launch: Option<&LaunchContext>,
    identity: &dyn IdentityResolver,
    sink: &dyn ExperimentStateSink,
) -> bool {
    if !experiments_allowed(effective) {
        return false;
    }
    let Some((api_base, api_key)) = mistral_provider_and_api_key(effective, credentials) else {
        return false;
    };
    let organization_id = identity
        .resolve(&api_base, &api_key, Some(EXPERIMENT_IDENTITY_TIMEOUT))
        .await
        .and_then(|identity| identity.organization_id().map(ToOwned::to_owned));
    let attributes = build_attributes(effective, &api_key, launch, organization_id);
    manager.initialize(&attributes).await;
    // The manager is fail-open and stays empty when the lookup produced
    // nothing, so there is no state to persist and nothing downstream to
    // refresh.
    let Some(state) = manager.export_state() else {
        return false;
    };
    sink.persist(state);
    true
}

/// Takes a resolved rollout back in without a lookup, which is what a resumed
/// or forked session does.
///
/// Both gates are read again here, so an operator who turned telemetry off
/// between two sessions is not enrolled by what the first one wrote.
///
/// Reference `hydrate_experiments_from_session`.
pub fn hydrate_experiments_from_session(
    effective: &Table,
    manager: &mut ExperimentManager,
    state: Option<EvalResponse>,
) -> bool {
    if !experiments_allowed(effective) {
        return false;
    }
    let Some(state) = state else {
        return false;
    };
    manager.hydrate(state);
    true
}

/// Whether this configuration allows a rollout to be resolved at all.
///
/// Both keys default to true, and both are read off the merged document, so the
/// answer follows a file edited between two sessions.
#[must_use]
pub fn experiments_allowed(effective: &Table) -> bool {
    let telemetry = effective
        .get("enable_telemetry")
        .and_then(toml::Value::as_bool)
        .unwrap_or(true);
    let experiments = effective
        .get("experiments")
        .and_then(toml::Value::as_table)
        .and_then(|table| table.get("enable"))
        .and_then(toml::Value::as_bool)
        .unwrap_or(true);
    telemetry && experiments
}

/// The Mistral provider's API base and the credential its variable resolves to.
///
/// A third-party provider answers nothing, which is the rule that keeps a
/// third-party credential from reaching a Mistral-controlled endpoint, and so
/// does a Mistral provider whose variable resolves to nothing.
///
/// Reference `get_mistral_provider_and_api_key`.
#[must_use]
pub fn mistral_provider_and_api_key(
    effective: &Table,
    credentials: &CredentialSource,
) -> Option<(String, String)> {
    let provider = mistral_provider(effective)?;
    let api_base = provider
        .get("api_base")
        .and_then(toml::Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let variable = provider
        .get("api_key_env_var")
        .and_then(toml::Value::as_str)?;
    if variable.is_empty() {
        return None;
    }
    let api_key = credentials(variable).filter(|value| !value.is_empty())?;
    Some((api_base, api_key))
}

/// The nine attributes the proxy evaluates a rollout against.
///
/// `userId` is the hash attribute, and it is the only derivative of the
/// credential that leaves this process. `custom_system_prompt` is a comparison
/// against the shipped default rather than a copy of the identifier, so a
/// rollout can target "this operator changed their prompt" without learning
/// which prompt they changed it to.
///
/// Reference `_build_attributes`.
#[must_use]
pub fn build_attributes(
    effective: &Table,
    api_key: &str,
    launch: Option<&LaunchContext>,
    organization_id: Option<String>,
) -> ExperimentAttributes {
    ExperimentAttributes {
        user_id: hash_api_key(api_key),
        entrypoint: launch.map_or_else(
            || UNKNOWN_ENTRYPOINT.to_owned(),
            |launch| launch.agent_entrypoint.clone(),
        ),
        agent_version: launch.map_or_else(
            || crate::telemetry::version().to_owned(),
            |launch| launch.agent_version.clone(),
        ),
        client_name: launch.map(|launch| launch.client_name.clone()),
        client_version: launch.map(|launch| launch.client_version.clone()),
        os: platform_id(),
        terminal_emulator: launch.and_then(|launch| launch.terminal_emulator.clone()),
        custom_system_prompt: custom_system_prompt(effective),
        organization_id,
    }
}

/// What a session launched by no declared adapter reports as its entrypoint.
///
/// Reference `_build_attributes`'s own fallback.
const UNKNOWN_ENTRYPOINT: &str = "unknown";

/// Whether the session runs a system prompt other than the shipped default.
///
/// The default is read off the published document rather than spelled here, so
/// a registry that moves it moves this comparison with it. A document that
/// names no prompt at all is running the default, which is what the reference's
/// own schema default produces.
fn custom_system_prompt(effective: &Table) -> bool {
    let declared = default_document()
        .get("system_prompt_id")
        .and_then(toml::Value::as_str)
        .map(ToOwned::to_owned);
    effective
        .get("system_prompt_id")
        .and_then(toml::Value::as_str)
        .is_some_and(|value| Some(value.to_owned()) != declared)
}
