//! The metadata census every event carries before its own payload.
//!
//! Reference `vibe/core/telemetry/types.py`, the two metadata models, and the
//! attachment counter of `build_metadata.py`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::TELEMETRY_ATTACHMENT_IMAGE;

/// What the process was launched as, supplied by the adapter that launched it.
/// Reference `LaunchContext`.
///
/// Serializing one is the reference's `telemetry_fields`: the terminal is
/// carried as null rather than dropped, and the census that consumes these
/// fields is what drops it. `None` means the adapter reports no terminal at
/// all; a terminal that cannot be identified reports `unknown` instead.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchContext {
    pub agent_entrypoint: String,
    pub agent_version: String,
    pub client_name: String,
    pub client_version: String,
    #[serde(default)]
    pub terminal_emulator: Option<String>,
}

/// Reference `TelemetryBaseMetadata`, in its declaration order. Every field is
/// optional and every absent one is dropped rather than sent as null, which is
/// the reference's `exclude_none`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TelemetryBaseMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_entrypoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_emulator: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    /// Filled from the exposures the experiments manager resolved, as upstream
    /// fills it. An unenrolled session resolves an empty map and the key is
    /// dropped rather than sent empty, which is axis 32 of `docs/parity.md`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub experiments: Option<BTreeMap<String, String>>,
    /// The confirmed exposures as records, one per experiment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub experiment_assignments: Option<Vec<ExperimentAssignment>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_plan: Option<String>,
    /// The attribute snapshot the proxy bucketed on, as it was posted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub experiment_attributes: Option<Map<String, Value>>,
    /// The backend serving the session, reference `ExperimentSurface`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness_backend: Option<String>,
}

/// One confirmed exposure, as a census reports it. Every optional field is
/// dropped when absent, which is the reference's recursive `exclude_none`.
///
/// Reference `ExperimentAssignment`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExperimentAssignment {
    pub experiment_id: String,
    pub experiment_name: String,
    pub variation_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variation_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_experiment: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash_attribute: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash_value: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feature_id: Option<String>,
}

/// Reference `ExperimentSurface.LEGACY`: the backend `--legacy-harness`
/// keeps a session on.
pub const HARNESS_LEGACY: &str = "legacy";

/// Reference `ExperimentSurface.UNIFIED`: the backend every other session
/// runs on.
pub const HARNESS_UNIFIED: &str = "unified";

/// The `harness_backend` a client reports for sessions on the selected
/// backend.
#[must_use]
pub fn harness_backend(use_unified: bool) -> &'static str {
    if use_unified {
        HARNESS_UNIFIED
    } else {
        HARNESS_LEGACY
    }
}

impl TelemetryBaseMetadata {
    /// The census as the properties an envelope carries.
    #[must_use]
    pub fn properties(&self) -> Map<String, Value> {
        properties_of(self)
    }
}

/// Reference `TelemetryCallType`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TelemetryCallType {
    #[default]
    MainCall,
    SecondaryCall,
    /// A smart approval's verdict, made on the utility model.
    SmartApprove,
    /// A session title, made on the utility model.
    TitleGeneration,
    /// A worktree name, made on the utility model.
    WorktreeTitle,
}

impl TelemetryCallType {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::MainCall => "main_call",
            Self::SecondaryCall => "secondary_call",
            Self::SmartApprove => "smart_approve",
            Self::TitleGeneration => "title_generation",
            Self::WorktreeTitle => "worktree_title",
        }
    }
}

/// Reference `TelemetryRequestMetadata`: the base census plus the four fields
/// a request-scoped event adds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TelemetryRequestMetadata {
    #[serde(flatten)]
    pub base: TelemetryBaseMetadata,
    pub call_type: TelemetryCallType,
    pub call_source: String,
    /// Reference `host_kind`, which declares the one value `local`.
    pub host_kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
}

impl TelemetryRequestMetadata {
    #[must_use]
    pub fn properties(&self) -> Map<String, Value> {
        properties_of(self)
    }
}

/// A serializable census as the object it serializes to. Neither census can
/// fail to serialize: every field is a string, a boolean or a map of strings.
fn properties_of<T: Serialize>(census: &T) -> Map<String, Value> {
    serde_json::to_value(census)
        .ok()
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default()
}

/// What the experiments path resolved for this session, as every census reads
/// it: the confirmed exposures, the attribute snapshot and the plan label.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExperimentCensus {
    pub assignments: Vec<ExperimentAssignment>,
    pub attributes: Option<Map<String, Value>>,
    /// The plan label the account lookup derived, which the snapshot's own
    /// plan fields take precedence over.
    pub user_plan: Option<String>,
}

impl ExperimentCensus {
    /// Reference `AgentLoop.user_plan`: the snapshot's plan fields decide the
    /// label whenever a snapshot exists, so the label and the attributes an
    /// event carries never disagree, and the label the lookup derived is the
    /// fallback.
    #[must_use]
    pub fn user_plan(&self) -> Option<String> {
        match &self.attributes {
            Some(attributes) => crate::whoami::resolve_user_plan(
                attributes.get("planType").and_then(Value::as_str),
                attributes.get("planName").and_then(Value::as_str),
            ),
            None => self.user_plan.clone(),
        }
    }

    /// Reference `{a.experiment_id: a.variation_name for a in assignments}`.
    #[must_use]
    pub fn experiments(&self) -> BTreeMap<String, String> {
        self.assignments
            .iter()
            .map(|assignment| {
                (
                    assignment.experiment_id.clone(),
                    assignment.variation_name.clone(),
                )
            })
            .collect()
    }
}

/// Reference `build_attachment_counts`: images are reported only when the
/// provider serving the request accepts them, and a message carrying none
/// reports no key rather than a zero.
#[must_use]
pub fn attachment_counts(images: usize, supports_images: bool) -> BTreeMap<String, u64> {
    let mut counts = BTreeMap::new();
    if supports_images && images > 0 {
        counts.insert(TELEMETRY_ATTACHMENT_IMAGE.to_owned(), images as u64);
    }
    counts
}
