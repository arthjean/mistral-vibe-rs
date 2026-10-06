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
//! Every session looks the rollout up when it starts. A resumed session first
//! takes back the variants it wrote, and a forked one the variants its parent
//! resolved, because the fork copies the metadata field; the lookup that
//! follows replaces them only when it answers.
//!
//! What a resolution changes is published rather than returned: the variants go
//! into the shared configuration layer and one load carries them to every cache
//! that follows a load, and the exposures, the attribute snapshot and the plan
//! label go into the handle the telemetry census reads on every event.
//!
//! The same lifecycle fetches what the organization enforces and reports the
//! outcome, which is the app server's own background task in the reference.
//!
//! Reference: `vibe/core/agent_loop/_loop.py`'s experiments task, its refresh
//! pair, `wait_until_ready` and its close order, `vibe/app_server/_runtime.py`'s
//! resume and fork, and `vibe/app_server/_legacy_session_runtime.py`'s
//! admin-config fetch.

use std::sync::{Arc, Mutex};

use vibe_core::config::LayeredConfig;
use vibe_core::experiments::{
    EvalResponse, ExperimentManager, ExperimentStateSink, PlanSources, RemoteEvalClient,
    hydrate_experiments_from_session, initialize_experiments,
};
use vibe_core::identity::{
    CachedIdentity, IdentityCache, IdentityFuture, IdentityResolver, IdentityResult,
};
use vibe_core::storage::SessionStore;
use vibe_core::telemetry::{ClientTelemetry, ExperimentExposures, HARNESS_LEGACY, LaunchContext};
use vibe_core::whoami::{WhoAmICache, WhoAmIResolver};

use crate::workspace::WorkspaceService;

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
    whoami: Arc<dyn WhoAmIResolver>,
    /// The production account cache behind `whoami`, which an account read
    /// populates or invalidates. Absent once a test replaced the resolver.
    account_cache: Option<Arc<WhoAmICache>>,
    manager: tokio::sync::Mutex<ExperimentManager>,
    exposures: ExperimentExposures,
    launch: Option<LaunchContext>,
    /// Where the admin-config outcome is reported, when the adapter has a
    /// telemetry client.
    telemetry: Option<Arc<dyn ClientTelemetry>>,
    /// The lookup in flight, held so a closing session can cancel it.
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// The admin-config fetch in flight, cancelled on close as well.
    admin: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Flips once the lookup settled, which is what [`Self::settle`] waits on.
    settled: tokio::sync::watch::Sender<bool>,
    /// When the session began initializing, which `vibe.ready` measures from.
    /// Reference `_init_start_time`.
    created: std::time::Instant,
    /// The workspace a replacing session's census is read from.
    workspace: WorkspaceService,
    /// The session whose context clears this enrollment follows, once
    /// [`Self::follow_resets`] named it.
    followed: Mutex<Option<FollowedSession>>,
    /// This enrollment, held weakly so a reset can detach work onto it.
    this: std::sync::OnceLock<std::sync::Weak<Self>>,
}

/// The session a reset replaces, and what its census is read against.
struct FollowedSession {
    session_id: String,
    working_directory: std::path::PathBuf,
    trust: bool,
}

impl SessionExperiments {
    /// The enrollment one service and one launch context resolve.
    ///
    /// The eval client is built from the merged configuration's own
    /// `[experiments]` table, so a document that blanks the host or the key
    /// produces a client that issues nothing.
    #[must_use]
    pub fn new(
        service: &WorkspaceService,
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
        let account_cache = Arc::new(WhoAmICache::production(config.harness_files().vibe_home()));
        Self {
            store: service.session_store(),
            config,
            credentials,
            identity,
            whoami: account_cache.clone(),
            account_cache: Some(account_cache),
            manager: tokio::sync::Mutex::new(ExperimentManager::new(client)),
            exposures,
            launch,
            telemetry: None,
            task: Mutex::new(None),
            admin: Mutex::new(None),
            settled: tokio::sync::watch::channel(false).0,
            created: std::time::Instant::now(),
            workspace: service.clone(),
            followed: Mutex::new(None),
            this: std::sync::OnceLock::new(),
        }
    }

    /// Reports the admin-config outcome through `telemetry`, which is what
    /// makes [`Self::start`] fetch the managed configuration at all.
    #[must_use]
    pub fn reporting_to(mut self, telemetry: Arc<dyn ClientTelemetry>) -> Self {
        self.telemetry = Some(telemetry);
        self
    }

    /// Replaces the account resolver, the seam the tests drive the plan
    /// through.
    #[must_use]
    pub fn resolving_account_through(mut self, whoami: Arc<dyn WhoAmIResolver>) -> Self {
        self.whoami = whoami;
        self.account_cache = None;
        self
    }

    /// Replaces the identity resolver, which is the seam the tests drive the
    /// organization attribute through.
    #[must_use]
    pub fn resolving_identity_through(mut self, identity: Arc<dyn IdentityResolver>) -> Self {
        self.identity = identity;
        self
    }

    /// Resolves this session's enrollment, its attribute snapshot and its
    /// plan.
    ///
    /// A session that carries variants takes them back first, which is the
    /// reference's `hydrate_experiments_from_session` on resume; the lookup
    /// `initialize_experiments` runs for every session afterward, as the
    /// reference's loop starts it whatever it was opened for.
    pub async fn resolve(&self, session_id: &str) {
        let effective = self
            .config
            .load()
            .map(|snapshot| snapshot.effective)
            .unwrap_or_default();
        let persisted = self.persisted_state(session_id);
        let mut manager = self.manager.lock().await;
        if persisted.is_some()
            && hydrate_experiments_from_session(&effective, &mut manager, persisted)
        {
            self.refresh(&manager);
        }
        let sink = MetadataSink {
            store: self.store.clone(),
            session_id: session_id.to_owned(),
        };
        let sources = PlanSources {
            identity: self.identity.as_ref(),
            whoami: self.whoami.as_ref(),
            harness: HARNESS_LEGACY,
        };
        let (changed, user_plan) = initialize_experiments(
            &effective,
            self.credentials.as_ref(),
            &mut manager,
            self.launch.as_ref(),
            &sources,
            &sink,
        )
        .await;
        self.exposures.publish_user_plan(user_plan);
        self.exposures.publish_attributes(
            manager
                .attributes()
                .and_then(|attributes| serde_json::to_value(attributes).ok())
                .and_then(|value| value.as_object().cloned()),
        );
        self.exposures.publish(manager.assignment_records());
        if changed {
            self.refresh(&manager);
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
            self.settled.send_replace(true);
            return;
        };
        let session = session_id.to_owned();
        let runtime = Arc::clone(self);
        *slot = Some(handle.spawn(async move {
            runtime.resolve(&session).await;
            runtime.settled.send_replace(true);
        }));
        drop(slot);
        if let Some(telemetry) = self.telemetry.clone() {
            let session = session_id.to_owned();
            let runtime = Arc::clone(self);
            *self
                .admin
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                Some(handle.spawn(async move {
                    let result = vibe_core::config::admin::refresh_admin_layer(
                        &runtime.config,
                        runtime.credentials.as_ref(),
                    )
                    .await;
                    if let Some(record) = result.report() {
                        telemetry.record(&record, Some(&session));
                    }
                }));
        }
    }

    /// Waits until the lookup [`Self::start`] began has settled, which is when
    /// the reference's `wait_until_ready` emits the lifecycle events: their
    /// census then carries what the lookup resolved. Answers at once when no
    /// lookup was started, and when one was cancelled.
    pub async fn settle(&self) {
        let started = self
            .task
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some();
        if !started {
            return;
        }
        let mut settled = self.settled.subscribe();
        drop(settled.wait_for(|done| *done).await);
    }

    /// Reports `vibe.ready` and then `vibe.new_session` once the lookup has
    /// settled, off the caller's path, so both carry what it resolved.
    /// Reference `wait_until_ready`. Nothing is reported without a telemetry
    /// client.
    pub fn announce_when_ready(
        self: &Arc<Self>,
        session_id: &str,
        census: vibe_core::telemetry::records::NewSession,
    ) {
        let (Some(telemetry), Ok(handle)) = (
            self.telemetry.clone(),
            tokio::runtime::Handle::try_current(),
        ) else {
            return;
        };
        let session = session_id.to_owned();
        let runtime = Arc::clone(self);
        handle.spawn(async move {
            runtime.settle().await;
            let init_duration_ms =
                u64::try_from(runtime.created.elapsed().as_millis()).unwrap_or(u64::MAX);
            telemetry.record(
                &vibe_core::telemetry::TelemetryRecord::Ready { init_duration_ms },
                Some(&session),
            );
            telemetry.record(
                &vibe_core::telemetry::TelemetryRecord::NewSession(census),
                Some(&session),
            );
        });
    }

    /// Reconciles the census with what an account read learned, once the
    /// lookup in flight has settled so this is the last writer. Reference
    /// `apply_account_whoami` and `clear_account_whoami`: a live plan replaces
    /// the plan fields of the snapshot and the label, a refused key clears
    /// them and the cached account, and a missing Mistral provider reports the
    /// sentinel.
    pub async fn apply_account(&self, lookup: &crate::workspace::AccountLookup) {
        use crate::workspace::AccountLookup;
        self.settle().await;
        let census = self.exposures.census();
        let mut attributes = census.attributes;
        match lookup {
            AccountLookup::NoMistralProvider => {
                self.exposures
                    .publish_user_plan(Some(vibe_core::whoami::NO_PLAN_DATA.to_owned()));
                return;
            }
            AccountLookup::Nothing => return,
            AccountLookup::Plan {
                console,
                key,
                result,
            } => {
                if let Some(cache) = &self.account_cache {
                    cache.populate(console, key, result.clone()).await;
                }
                if let Some(attributes) = attributes.as_mut() {
                    let text = |value: Option<&str>| {
                        value.map_or(serde_json::Value::Null, |value| value.into())
                    };
                    for (field, value) in [
                        ("planType", text(Some(result.plan_type.as_str()))),
                        ("planName", text(Some(&result.plan_name))),
                        ("customerId", text(result.customer_id.as_deref())),
                        (
                            "organizationKind",
                            text(result.organization_kind.as_deref()),
                        ),
                    ] {
                        if value.is_null() {
                            attributes.remove(field);
                        } else {
                            attributes.insert(field.to_owned(), value);
                        }
                    }
                }
                self.exposures
                    .publish_user_plan(vibe_core::whoami::derive_user_plan(Some(result)));
            }
            AccountLookup::Unauthorized { key } => {
                if let Some(cache) = &self.account_cache {
                    cache.invalidate(key).await;
                }
                if let Some(attributes) = attributes.as_mut() {
                    for field in ["planType", "planName", "customerId", "organizationKind"] {
                        attributes.remove(field);
                    }
                }
                self.exposures.publish_user_plan(None);
            }
        }
        self.exposures.publish_attributes(attributes);
    }

    /// Follows the context clears of `session_id` through `resets`, so the
    /// session that replaces it resolves its rollout and reports itself.
    pub fn follow_resets(
        self: &Arc<Self>,
        resets: &vibe_core::telemetry::SessionResets,
        session_id: &str,
        working_directory: &std::path::Path,
        trust: bool,
    ) {
        *self
            .followed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(FollowedSession {
            session_id: session_id.to_owned(),
            working_directory: working_directory.to_path_buf(),
            trust,
        });
        let _ = self.this.set(Arc::downgrade(self));
        let this: Arc<dyn vibe_core::telemetry::SessionReset> = Arc::clone(self) as _;
        resets.register(Arc::downgrade(&this));
    }

    /// Reports `vibe.session_closed`. Reference
    /// `emit_session_closed_telemetry`, raised before the session closes so
    /// the census still names it.
    pub fn report_closed(&self, session_id: &str) {
        if let Some(telemetry) = &self.telemetry {
            telemetry.record(
                &vibe_core::telemetry::TelemetryRecord::SessionClosed,
                Some(session_id),
            );
        }
    }

    /// Cancels the lookup and the fetch in flight and releases the client, in
    /// that order.
    ///
    /// Reference `aclose` cancels its experiments task before closing the
    /// manager, so a shutdown never waits on a request that is still going.
    pub async fn close(&self) {
        for slot in [&self.task, &self.admin] {
            let task = slot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            if let Some(task) = task {
                task.abort();
                // A cancelled task is the expected outcome here; the join is
                // what makes the close ordered rather than racing the abort.
                drop(task.await);
            }
        }
        self.settled.send_replace(true);
        self.manager.lock().await.close().await;
    }

    /// What the session already resolved, if it carries anything.
    fn persisted_state(&self, session_id: &str) -> Option<EvalResponse> {
        // `open`, not `metadata`: a session is held before its first save.
        let metadata = self.store.open(session_id).ok()?.metadata;
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
    /// also refreshes the system prompt here; a session here recomposes its
    /// prompt on its next turn, because the merged settings it was composed
    /// under no longer match.
    fn refresh(&self, manager: &ExperimentManager) {
        let directories = self.config.harness_files().prompts_dirs();
        self.config
            .set_experiment_variants(&manager.config_variants(), &|prompt_id| {
                vibe_core::system_prompt::load_system_prompt(prompt_id, &directories).is_ok()
            });
        drop(self.config.load());
        self.exposures.publish(manager.assignment_records());
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
        let Ok(mut metadata) = self
            .store
            .open(&self.session_id)
            .map(|hydrated| hydrated.metadata)
        else {
            return;
        };
        let Ok(value) = serde_json::to_value(state) else {
            return;
        };
        metadata.experiment_state = value;
        drop(self.store.update_metadata(&metadata));
    }
}

impl vibe_core::telemetry::SessionReset for SessionExperiments {
    /// Reference `_reset_session` after the close: the rollout is looked up
    /// again for the new session, which then reports `vibe.new_session`, off
    /// the caller's path.
    fn reset(&self, from_session_id: &str, to_session_id: &str) -> bool {
        let (working_directory, trust) = {
            let mut followed = self
                .followed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(followed) = followed
                .as_mut()
                .filter(|followed| followed.session_id == from_session_id)
            else {
                return false;
            };
            to_session_id.clone_into(&mut followed.session_id);
            (followed.working_directory.clone(), followed.trust)
        };
        let (Some(this), Ok(handle)) = (
            self.this.get().and_then(std::sync::Weak::upgrade),
            tokio::runtime::Handle::try_current(),
        ) else {
            return true;
        };
        let session = to_session_id.to_owned();
        handle.spawn(async move {
            this.settle().await;
            this.resolve(&session).await;
            if let Some(telemetry) = this.telemetry.as_ref() {
                let census = this.workspace.session_census(&working_directory, trust);
                telemetry.record(
                    &vibe_core::telemetry::TelemetryRecord::NewSession(census),
                    Some(&session),
                );
            }
        });
        true
    }
}
