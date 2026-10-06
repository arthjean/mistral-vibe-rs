//! What a session does with a rollout: when it is allowed to ask, what it
//! posts, what it keeps, and what it reads back on a resume.
//!
//! One gate stops everything: `enable_telemetry`, read off the merged
//! configuration rather than latched at startup. Past it, the caller's
//! identity and account are resolved whenever a Mistral credential exists,
//! because the plan and organization they carry segment every telemetry
//! event; `experiments.enable` then decides only whether the evaluation itself
//! runs. A session with no Mistral provider at all reports the
//! [`NO_PLAN_DATA`] sentinel instead, and one whose Mistral provider has no key
//! reports nothing.
//!
//! What the lookup posts about the caller is built here as well. `userId` is
//! the identifier the identity endpoint answers, never the credential.
//!
//! Reference: `vibe/core/experiments/session.py` at the pinned commit.

use std::time::Duration;

use toml::Table;

use crate::config::registry::default_document;
use crate::identity::{IdentityResolver, IdentityResult};
use crate::telemetry::{LaunchContext, mistral_provider, platform_arch, platform_id};
use crate::whoami::{NO_PLAN_DATA, WhoAmIResolver, WhoAmIResult, derive_user_plan};

use super::manager::ExperimentManager;
use super::models::{EvalResponse, ExperimentAttributes};

/// How long the identity and account requests are each given.
///
/// Reference `EXPERIMENT_IDENTITY_TIMEOUT_S`, held here as the duration a
/// request is issued with, so the number is spelled once. Both requests fail
/// open: an answer that cannot be resolved costs attributes, not a session.
pub const EXPERIMENT_IDENTITY_TIMEOUT: Duration = Duration::from_millis(10_000);

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

/// Where the identity and the account a snapshot is built from are read.
pub struct PlanSources<'a> {
    pub identity: &'a dyn IdentityResolver,
    pub whoami: &'a dyn WhoAmIResolver,
    /// The backend serving the session, reference `ExperimentSurface`.
    pub harness: &'a str,
}

/// The attribute snapshot and the plan label, resolved from the identity and
/// the account independently of any session state.
///
/// - No Mistral provider at all: a sentinel snapshot whose plan fields are
///   [`NO_PLAN_DATA`], and that label.
/// - A Mistral provider whose key does not resolve: nothing, so neither field
///   is reported.
/// - Otherwise the identity and the account, fetched concurrently.
///
/// Reference `_fetch_plan_attributes`.
async fn fetch_plan_attributes(
    effective: &Table,
    credentials: &CredentialSource,
    launch: Option<&LaunchContext>,
    sources: &PlanSources<'_>,
) -> (Option<ExperimentAttributes>, Option<String>) {
    let Some((api_base, api_key)) = mistral_provider_and_api_key(effective, credentials) else {
        if mistral_provider(effective).is_none() {
            let mut sentinel = build_attributes(effective, launch, sources.harness, None, None);
            sentinel.plan_type = Some(NO_PLAN_DATA.to_owned());
            sentinel.plan_name = Some(NO_PLAN_DATA.to_owned());
            return (Some(sentinel), Some(NO_PLAN_DATA.to_owned()));
        }
        return (None, None);
    };
    let console = effective
        .get("console_base_url")
        .and_then(toml::Value::as_str)
        .map_or_else(|| DEFAULT_CONSOLE_BASE_URL.to_owned(), ToOwned::to_owned);
    let (identity, whoami) = tokio::join!(
        sources
            .identity
            .resolve(&api_base, &api_key, Some(EXPERIMENT_IDENTITY_TIMEOUT)),
        sources
            .whoami
            .resolve(&console, &api_key, Some(EXPERIMENT_IDENTITY_TIMEOUT)),
    );
    let attributes = build_attributes(
        effective,
        launch,
        sources.harness,
        identity.as_ref(),
        whoami.as_ref(),
    );
    (Some(attributes), derive_user_plan(whoami.as_ref()))
}

/// The console the account endpoint is served from when the document names
/// none. Reference `VibeConfigSchema.console_base_url`'s default.
const DEFAULT_CONSOLE_BASE_URL: &str = "https://console.mistral.ai";

/// Resolves the snapshot and the plan, then looks the rollout up for this
/// session and records what it resolved.
///
/// Answers whether the variants changed, which is what decides a configuration
/// refresh, beside the plan label the account lookup derived. A missing
/// credential, the sentinel, the experiments opt-out and a failed eval all
/// answer `false`, because in every one of those cases the manager is still on
/// its declared defaults; the opt-out and the sentinel still leave the snapshot
/// on the manager for telemetry.
///
/// Reference `initialize_experiments`.
pub async fn initialize_experiments(
    effective: &Table,
    credentials: &CredentialSource,
    manager: &mut ExperimentManager,
    launch: Option<&LaunchContext>,
    sources: &PlanSources<'_>,
    sink: &dyn ExperimentStateSink,
) -> (bool, Option<String>) {
    if !telemetry_enabled(effective) {
        return (false, None);
    }
    let (attributes, user_plan) =
        fetch_plan_attributes(effective, credentials, launch, sources).await;
    let Some(attributes) = attributes else {
        return (false, user_plan);
    };
    if user_plan.as_deref() == Some(NO_PLAN_DATA) || !experiments_enabled(effective) {
        manager.set_attributes(attributes);
        return (false, user_plan);
    }
    manager.initialize(&attributes).await;
    // The manager is fail-open and stays empty when the lookup produced
    // nothing, so there is no state to persist and nothing to refresh.
    let Some(state) = manager.export_state() else {
        return (false, user_plan);
    };
    sink.persist(state);
    (true, user_plan)
}

/// Rebuilds the snapshot and the plan without a lookup, which is what a
/// resumed session does: its variants stay frozen while its plan reflects the
/// current account.
///
/// Reference `resolve_plan_attributes`.
pub async fn resolve_plan_attributes(
    effective: &Table,
    credentials: &CredentialSource,
    manager: &mut ExperimentManager,
    launch: Option<&LaunchContext>,
    sources: &PlanSources<'_>,
) -> Option<String> {
    if !telemetry_enabled(effective) {
        return None;
    }
    let (attributes, user_plan) =
        fetch_plan_attributes(effective, credentials, launch, sources).await;
    if let Some(attributes) = attributes {
        manager.set_attributes(attributes);
    }
    user_plan
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
    telemetry_enabled(effective) && experiments_enabled(effective)
}

fn telemetry_enabled(effective: &Table) -> bool {
    effective
        .get("enable_telemetry")
        .and_then(toml::Value::as_bool)
        .unwrap_or(true)
}

fn experiments_enabled(effective: &Table) -> bool {
    effective
        .get("experiments")
        .and_then(toml::Value::as_table)
        .and_then(|table| table.get("enable"))
        .and_then(toml::Value::as_bool)
        .unwrap_or(true)
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

/// The attributes the proxy evaluates a rollout against.
///
/// `userId` is the identity's own identifier, and the organization and the
/// workspace come from the same answer; the plan, its kind and the customer
/// come from the account. `custom_system_prompt` is a comparison against the
/// shipped default rather than a copy of the identifier, so a rollout can
/// target "this operator changed their prompt" without learning which prompt
/// they changed it to.
///
/// Reference `_build_attributes`.
#[must_use]
pub fn build_attributes(
    effective: &Table,
    launch: Option<&LaunchContext>,
    harness: &str,
    identity: Option<&IdentityResult>,
    whoami: Option<&WhoAmIResult>,
) -> ExperimentAttributes {
    ExperimentAttributes {
        user_id: identity.map(|identity| identity.id.clone()),
        entrypoint: launch.map_or_else(
            || UNKNOWN_ENTRYPOINT.to_owned(),
            |launch| launch.agent_entrypoint.clone(),
        ),
        harness: harness.to_owned(),
        agent_version: launch.map_or_else(
            || crate::telemetry::version().to_owned(),
            |launch| launch.agent_version.clone(),
        ),
        client_name: launch.map(|launch| launch.client_name.clone()),
        client_version: launch.map(|launch| launch.client_version.clone()),
        os: platform_id(),
        arch: platform_arch(),
        terminal_emulator: launch.and_then(|launch| launch.terminal_emulator.clone()),
        custom_system_prompt: custom_system_prompt(effective),
        organization_id: identity
            .and_then(IdentityResult::organization_id)
            .map(ToOwned::to_owned),
        organization_kind: whoami.and_then(|whoami| whoami.organization_kind.clone()),
        workspace_id: identity
            .and_then(|identity| identity.workspace.as_ref())
            .map(|workspace| workspace.id.clone()),
        customer_id: whoami.and_then(|whoami| whoami.customer_id.clone()),
        plan_type: whoami.map(|whoami| whoami.plan_type.as_str().to_owned()),
        plan_name: whoami.map(|whoami| whoami.plan_name.clone()),
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
