//! What every event of this process reports, as handles the sessions publish
//! into after the client is built.
//!
//! Reference `TelemetryClient`'s getters (`vibe/core/telemetry/send.py`), which
//! `build_base_metadata` and `build_request_metadata`
//! (`vibe/core/telemetry/build_metadata.py`) read on every send.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use serde_json::{Map, Value};

use super::{
    ExperimentAssignment, ExperimentCensus, LaunchContext, TELEMETRY_CALL_SOURCE,
    TELEMETRY_HOST_KIND, TelemetryBaseMetadata, TelemetryCallType, TelemetryRequestMetadata,
    VERSION, platform_arch, platform_id, platform_version,
};
use crate::sync::lock;

/// The experiment state every event of this process reports, as the handle a
/// session publishes into and a census reads out of.
///
/// A rollout is resolved off the startup path and lands after the telemetry
/// client is already built, so the state cannot be a value the context was
/// constructed with. The reference reads it through getters closed over its
/// agent loop and experiment manager; this is those getters, as a handle every
/// clone of the context shares. A context nobody publishes into reports
/// nothing, which is what a disabled gate and a missing credential produce.
#[derive(Debug, Clone, Default)]
pub struct ExperimentExposures(Arc<Mutex<ExperimentCensus>>);

impl ExperimentExposures {
    fn with<R>(&self, read: impl FnOnce(&mut ExperimentCensus) -> R) -> R {
        read(&mut lock(&self.0))
    }

    /// Replaces the confirmed exposures every later event reports.
    ///
    /// Reference `ExperimentManager.assignments` is re-read on every send, so
    /// the last publication is what an event carries.
    pub fn publish(&self, assignments: Vec<ExperimentAssignment>) {
        self.with(|census| census.assignments = assignments);
    }

    /// Replaces the attribute snapshot. Reference
    /// `ExperimentManager.set_attributes`.
    pub fn publish_attributes(&self, attributes: Option<Map<String, Value>>) {
        self.with(|census| census.attributes = attributes);
    }

    /// Replaces the plan label the lookup derived. Reference
    /// `AgentLoop.set_user_plan`.
    pub fn publish_user_plan(&self, user_plan: Option<String>) {
        self.with(|census| census.user_plan = user_plan);
    }

    /// Forgets everything, which is what a reset session starts from.
    pub fn clear(&self) {
        self.with(|census| *census = ExperimentCensus::default());
    }

    /// What an event built now would report.
    #[must_use]
    pub fn census(&self) -> ExperimentCensus {
        self.with(|census| census.clone())
    }

    /// The confirmed exposures an event built now would report, by experiment.
    #[must_use]
    pub fn resolved(&self) -> BTreeMap<String, String> {
        self.with(|census| census.experiments())
    }
}

/// Two handles are equal when they report the same state, which is what a
/// census comparison is about.
impl PartialEq for ExperimentExposures {
    fn eq(&self, other: &Self) -> bool {
        self.census() == other.census()
    }
}

impl Eq for ExperimentExposures {}

/// The launch a client declared once it connected, shared by every clone of
/// the context that reports it.
#[derive(Debug, Clone, Default)]
pub struct DeclaredLaunch(Arc<Mutex<Option<LaunchContext>>>);

impl DeclaredLaunch {
    /// Replaces what every later event reports.
    pub fn declare(&self, launch: LaunchContext) {
        *lock(&self.0) = Some(launch);
    }

    #[must_use]
    pub fn get(&self) -> Option<LaunchContext> {
        lock(&self.0).clone()
    }
}

impl PartialEq for DeclaredLaunch {
    fn eq(&self, other: &Self) -> bool {
        self.get() == other.get()
    }
}

impl Eq for DeclaredLaunch {}

/// The parent each session reports, bound whenever a session takes a new
/// identity: a clear starts one with none, a fork names the session it left,
/// and a resume takes what the session recorded. Reference
/// `parent_session_id_getter`, which reads the loop's own field as
/// `_reset_session` and `resume` rebind it.
#[derive(Debug, Clone, Default)]
pub struct SessionParents(Arc<Mutex<BTreeMap<String, Option<String>>>>);

impl SessionParents {
    pub fn bind(&self, session_id: &str, parent_session_id: Option<String>) {
        lock(&self.0).insert(session_id.to_owned(), parent_session_id);
    }

    #[must_use]
    pub fn get(&self, session_id: &str) -> Option<Option<String>> {
        lock(&self.0).get(session_id).cloned()
    }
}

impl SessionParents {
    fn snapshot(&self) -> BTreeMap<String, Option<String>> {
        lock(&self.0).clone()
    }
}

impl PartialEq for SessionParents {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0) || self.snapshot() == other.snapshot()
    }
}

impl Eq for SessionParents {}

/// What takes over when a session's context is cleared into a new one: the
/// rollout is resolved again for the new session, which then reports itself.
pub trait SessionReset: Send + Sync {
    /// Answers whether this reset owned `from_session_id` and took it over.
    fn reset(&self, from_session_id: &str, to_session_id: &str) -> bool;
}

/// Every [`SessionReset`] of this process, which the observer hands a cleared
/// session to. Held weakly, so a session that closed simply stops answering.
#[derive(Clone, Default)]
pub struct SessionResets(Arc<Mutex<Vec<std::sync::Weak<dyn SessionReset>>>>);

impl SessionResets {
    pub fn register(&self, reset: std::sync::Weak<dyn SessionReset>) {
        let mut resets = lock(&self.0);
        resets.retain(|reset| reset.strong_count() > 0);
        resets.push(reset);
    }

    /// Hands the reset of `from_session_id` to whichever reset owns it.
    pub fn reset(&self, from_session_id: &str, to_session_id: &str) -> bool {
        let resets = lock(&self.0)
            .iter()
            .filter_map(std::sync::Weak::upgrade)
            .collect::<Vec<_>>();
        resets
            .iter()
            .any(|reset| reset.reset(from_session_id, to_session_id))
    }
}

impl std::fmt::Debug for SessionResets {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SessionResets")
    }
}

impl PartialEq for SessionResets {
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}

impl Eq for SessionResets {}

/// What every event of this process reports before its own payload. Reference
/// `TelemetryClient.__init__`'s six getters, held as values because this port
/// reads the session from the event it is projecting rather than from a
/// getter. The exposures are the exception: they resolve after the client is
/// built, so they are read through [`ExperimentExposures`] on every send, as
/// the reference reads them through its own getter.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TelemetryContext {
    pub launch: Option<LaunchContext>,
    /// What the client declared about itself once it connected, which takes
    /// precedence over [`Self::launch`]. Reference
    /// `_build_launch_context_from_services` reads the client descriptor
    /// lazily, after the handshake, which is what an editor's name reaches the
    /// census through.
    pub declared_launch: DeclaredLaunch,
    /// The parent a session reports when none was bound for it.
    pub parent_session_id: Option<String>,
    pub session_parents: SessionParents,
    pub session_resets: SessionResets,
    pub experiments: ExperimentExposures,
    /// Reference `TelemetryClient._harness_backend`, fixed when the client is
    /// built.
    pub harness_backend: Option<String>,
}

impl TelemetryContext {
    /// The launch every event reports: what the client declared, or what the
    /// adapter was built with.
    #[must_use]
    pub fn resolved_launch(&self) -> Option<LaunchContext> {
        self.declared_launch.get().or_else(|| self.launch.clone())
    }

    /// The parent `session_id` reports: the one bound for it, or the
    /// context's own.
    #[must_use]
    pub fn parent_of(&self, session_id: Option<&str>) -> Option<String> {
        session_id
            .and_then(|session_id| self.session_parents.get(session_id))
            .unwrap_or_else(|| self.parent_session_id.clone())
    }

    /// Reference `build_base_metadata`.
    #[must_use]
    pub fn base_metadata(&self, session_id: Option<&str>) -> TelemetryBaseMetadata {
        let resolved = self.resolved_launch();
        let launch = resolved.as_ref();
        let census = self.experiments.census();
        TelemetryBaseMetadata {
            agent_entrypoint: launch.map(|launch| launch.agent_entrypoint.clone()),
            agent_version: launch.map(|launch| launch.agent_version.clone()),
            client_name: launch.map(|launch| launch.client_name.clone()),
            client_version: launch.map(|launch| launch.client_version.clone()),
            os: Some(platform_id()),
            arch: Some(platform_arch()),
            os_version: platform_version(),
            version: Some(VERSION.to_owned()),
            terminal_emulator: launch.and_then(|launch| launch.terminal_emulator.clone()),
            session_id: session_id.map(ToOwned::to_owned),
            parent_session_id: self.parent_of(session_id),
            // Reference `experiments or None`: an empty map is absent rather
            // than an empty object, which is what an unenrolled session sends,
            // and the same holds for the records and the snapshot.
            experiments: Some(census.experiments()).filter(|map| !map.is_empty()),
            user_plan: census.user_plan(),
            experiment_assignments: Some(census.assignments).filter(|list| !list.is_empty()),
            experiment_attributes: census.attributes.filter(|map| !map.is_empty()),
            harness_backend: self.harness_backend.clone(),
        }
    }

    /// Reference `build_request_metadata`.
    #[must_use]
    pub fn request_metadata(
        &self,
        session_id: Option<&str>,
        call_type: TelemetryCallType,
        message_id: Option<String>,
    ) -> TelemetryRequestMetadata {
        TelemetryRequestMetadata {
            base: TelemetryBaseMetadata {
                // The reference's request builder sets the plan and leaves the
                // experiment fields and the backend unset.
                experiments: None,
                experiment_assignments: None,
                experiment_attributes: None,
                harness_backend: None,
                ..self.base_metadata(session_id)
            },
            call_type,
            call_source: TELEMETRY_CALL_SOURCE.to_owned(),
            host_kind: TELEMETRY_HOST_KIND.to_owned(),
            message_id,
        }
    }
}
