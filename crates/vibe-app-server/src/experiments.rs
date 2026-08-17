//! A session's rollout, from the lookup it runs off the startup path to the
//! configuration and the telemetry census it changes.
//!
//! [`vibe_core::experiments`] owns what a variant means; this owns when a
//! session asks and what it does with the answer. Three rules shape it, and all
//! three come from the reference.
//!
//! The lookup never blocks a session. It runs in a detached task, and a session
//! that closes while one is in flight cancels the task before it closes the
//! client, so shutdown is bounded by the cancellation rather than by the
//! request.
//!
//! A session resolves its enrollment once. A resumed session reads what it
//! wrote, and a forked one inherits what its parent resolved, because the fork
//! copies the metadata field; neither issues a second request. Only a session
//! that carries no state at all looks one up.
//!
//! What a resolution changes is published rather than returned: the variants go
//! into the shared configuration layer and one load carries them to every cache
//! that follows a load, and the confirmed exposures go into the handle the
//! telemetry census reads on every event.
//!
//! Reference: `vibe/core/agent_loop/_loop.py`'s experiments task, its refresh
//! pair and its close order, and `vibe/app_server/_runtime.py`'s resume and
//! fork.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use vibe_core::config::LayeredConfig;
use vibe_core::experiments::{
    EvalResponse, ExperimentManager, ExperimentStateSink, RemoteEvalClient,
    hydrate_experiments_from_session, initialize_experiments,
};
use vibe_core::identity::{
    CachedIdentity, IdentityCache, IdentityFuture, IdentityResolver, IdentityResult,
};
use vibe_core::prompt::PromptResolver;
use vibe_core::storage::SessionStore;
use vibe_core::telemetry::{ExperimentExposures, LaunchContext};

use crate::release3::Release3Service;

#[cfg(test)]
mod tests;

/// How a variable becomes a credential, supplied by the adapter that already
/// resolved one for its provider.
pub type Credentials = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// One session's enrollment.
pub struct SessionExperiments {
    config: LayeredConfig,
    store: SessionStore,
    credentials: Credentials,
    identity: Arc<dyn IdentityResolver>,
    manager: tokio::sync::Mutex<ExperimentManager>,
    exposures: ExperimentExposures,
    launch: Option<LaunchContext>,
    /// Where a prompt variant is validated against, kept as roots because a
    /// resolver is cheap to build and a session's prompt directories can be
    /// written to between two loads.
    prompt_roots: PromptRoots,
    /// The lookup in flight, held so a closing session can cancel it.
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

struct PromptRoots {
    project: Vec<PathBuf>,
    user: Vec<PathBuf>,
    project_trusted: bool,
}

impl SessionExperiments {
    /// The enrollment one service and one launch context resolve.
    ///
    /// The eval client is built from the merged configuration's own
    /// `[experiments]` table, so a document that blanks the host or the key
    /// produces a client that issues nothing.
    #[must_use]
    pub fn new(
        service: &Release3Service,
        credentials: Credentials,
        launch: Option<LaunchContext>,
        exposures: ExperimentExposures,
    ) -> Self {
        let config = service.layered_config();
        let settings = config
            .load()
            .ok()
            .and_then(|snapshot| {
                snapshot
                    .effective
                    .get("experiments")
                    .and_then(toml::Value::as_table)
                    .cloned()
            })
            .unwrap_or_default();
        let read = |key: &str| {
            settings
                .get(key)
                .and_then(toml::Value::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        let client = RemoteEvalClient::from_settings(&read("api_host"), &read("client_key"));
        let identity: Arc<dyn IdentityResolver> =
            match CachedIdentity::production(Arc::new(IdentityCache::new())) {
                Some(resolver) => Arc::new(resolver),
                // An HTTP client that will not build is one more way to get no
                // organization, which a fail-open lookup already handles.
                None => Arc::new(NoIdentity),
            };
        Self {
            store: service.session_store(),
            prompt_roots: PromptRoots {
                project: service.project_prompt_roots(),
                user: vec![service.vibe_home().join("extensions/prompts")],
                project_trusted: service.project_trusted(),
            },
            config,
            credentials,
            identity,
            manager: tokio::sync::Mutex::new(ExperimentManager::new(client)),
            exposures,
            launch,
            task: Mutex::new(None),
        }
    }

    /// Replaces the identity resolver, which is the seam the tests drive the
    /// organization attribute through.
    #[must_use]
    pub fn resolving_identity_through(mut self, identity: Arc<dyn IdentityResolver>) -> Self {
        self.identity = identity;
        self
    }

    /// The handle the telemetry census reads its exposures out of.
    #[must_use]
    pub fn exposures(&self) -> &ExperimentExposures {
        &self.exposures
    }

    /// Resolves this session's enrollment: what it already carries, or what a
    /// lookup answers.
    ///
    /// Reference splits these into `hydrate_experiments_from_session` and
    /// `initialize_experiments`, called from the resume path and the new-session
    /// path respectively. The decision is the same one, and it is made here off
    /// the state the session metadata carries, which is what a fork inherits
    /// from its parent.
    pub async fn resolve(&self, session_id: &str) {
        let effective = self
            .config
            .load()
            .map(|snapshot| snapshot.effective)
            .unwrap_or_default();
        let persisted = self.persisted_state(session_id);
        let mut manager = self.manager.lock().await;
        let changed = match persisted {
            Some(state) => hydrate_experiments_from_session(&effective, &mut manager, Some(state)),
            None => {
                let sink = MetadataSink {
                    store: self.store.clone(),
                    session_id: session_id.to_owned(),
                };
                initialize_experiments(
                    &effective,
                    self.credentials.as_ref(),
                    &mut manager,
                    self.launch.as_ref(),
                    self.identity.as_ref(),
                    &sink,
                )
                .await
            }
        };
        if changed {
            self.publish(&manager);
        }
    }

    /// Starts the resolution off the caller's path.
    ///
    /// Reference `start_initialize_experiments` creates a detached task and
    /// returns immediately, which is what keeps time to first prompt
    /// independent of a rollout service. Calling this twice starts one lookup:
    /// the second call sees a task already held.
    pub fn start(self: &Arc<Self>, session_id: &str) {
        let mut slot = self
            .task
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if slot.is_some() {
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            // No runtime to detach onto, which is a caller that never awaits
            // anything: there is nothing to start and nothing to report.
            return;
        };
        let session = session_id.to_owned();
        let runtime = Arc::clone(self);
        *slot = Some(handle.spawn(async move {
            runtime.resolve(&session).await;
        }));
    }

    /// Cancels the lookup in flight and releases the client, in that order.
    ///
    /// Reference `aclose` cancels its experiments task before closing the
    /// manager, so a shutdown never waits on a request that is still going.
    pub async fn close(&self) {
        let task = self
            .task
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(task) = task {
            task.abort();
            // A cancelled task is the expected outcome here; the join is what
            // makes the close ordered rather than racing the abort.
            drop(task.await);
        }
        self.manager.lock().await.close().await;
    }

    /// What the session already resolved, if it carries anything.
    fn persisted_state(&self, session_id: &str) -> Option<EvalResponse> {
        let metadata = self.store.metadata(session_id).ok()?;
        if metadata.experiment_state.is_null() {
            return None;
        }
        serde_json::from_value(metadata.experiment_state).ok()
    }

    /// Carries a resolved rollout to the two surfaces that read one.
    ///
    /// Reference `_sync_growthbook_layer_variants` followed by
    /// `refresh_config`: the variants go into the layer, and the load is what
    /// republishes the merged document to the caches that follow one, which is
    /// how the managed shell family reaches the next registration. Reference
    /// also refreshes the system prompt here; this port has nothing to refresh,
    /// because the merged `system_prompt_id` has no consumer composing a
    /// session's prompt yet, which is the gap the "System prompt and project
    /// context" row of `docs/parity.md` records. The assignment still lands in
    /// the layer, so it takes effect with that consumer rather than needing a
    /// second write here.
    fn publish(&self, manager: &ExperimentManager) {
        let resolver = PromptResolver::new(
            self.prompt_roots.project.clone(),
            self.prompt_roots.user.clone(),
            std::collections::BTreeMap::new(),
            self.prompt_roots.project_trusted,
        );
        self.config
            .set_experiment_variants(&manager.config_variants(), &|prompt_id| {
                resolver.resolve(prompt_id).is_ok()
            });
        drop(self.config.load());
        self.exposures.publish(manager.assignments());
    }
}

/// A resolver for a process that cannot build an HTTP client.
struct NoIdentity;

impl IdentityResolver for NoIdentity {
    fn resolve<'a>(
        &'a self,
        _base_url: &'a str,
        _api_key: &'a str,
        _timeout: Option<std::time::Duration>,
    ) -> IdentityFuture<'a, Option<IdentityResult>> {
        Box::pin(std::future::ready(None))
    }
}

/// Where a resolved rollout is written so the next session that reads this one
/// resolves the same variants.
///
/// Reference `SessionLogger.persist_experiments` writes one metadata field and
/// its caller suppresses every failure, so a session whose metadata cannot be
/// read or written still runs on what it resolved in memory.
struct MetadataSink {
    store: SessionStore,
    session_id: String,
}

impl ExperimentStateSink for MetadataSink {
    fn persist(&self, state: &EvalResponse) {
        let Ok(mut metadata) = self.store.metadata(&self.session_id) else {
            return;
        };
        let Ok(value) = serde_json::to_value(state) else {
            return;
        };
        metadata.experiment_state = value;
        drop(self.store.update_metadata(&metadata));
    }
}
