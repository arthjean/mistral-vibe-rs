//! What the client sends and what the remote evaluation endpoint answers.
//!
//! Every model here is deliberately tolerant: an unknown field is ignored
//! rather than rejected. That is the opposite of the rule `vibe-protocol`
//! envelopes follow, where `deny_unknown_fields` is what lets the untagged
//! `Envelope` discriminate its variants, and the difference is not an
//! oversight. The eval response is written by the GrowthBook proxy, not by this
//! product: it carries a scrubbed `experiments` block beside `features`, and a
//! rule the proxy resolved keeps whatever bookkeeping the proxy version
//! attached to it. The reference declares `extra="ignore"` on all six models
//! for that reason, so a payload from a newer proxy resolves rather than
//! failing the whole lookup and dropping the user back to defaults.
//!
//! The values a rule forces are [`JsonValue`] rather than [`serde_json::Value`]
//! because the reference answers an object-valued variant with `json.dumps` of
//! the decoded payload, and that string carries the key order the wire carried.
//! [`super::json`] explains what a sorted re-encode would change.
//!
//! Reference: `vibe/core/experiments/models.py` at the pinned commit.

use serde::{Deserialize, Serialize};

use super::json::{JsonValue, OrderedMap};

/// The attributes the client posts for the proxy to evaluate against.
///
/// `user_id` is the GrowthBook hash attribute, and it is a digest rather than
/// an identity: [`super::manager::hash_api_key`] derives it from the Mistral
/// API key so bucketing is stable per user without the credential leaving the
/// process. The wire spelling is `userId`, which is the attribute name a
/// rollout selects in its "assign variation based on attribute" setting, so the
/// rename is part of the contract rather than a style choice.
///
/// Reference `ExperimentAttributes`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExperimentAttributes {
    #[serde(rename = "userId")]
    pub user_id: String,
    pub entrypoint: String,
    pub agent_version: String,
    /// Absent rather than null on the wire, which is what the reference's
    /// `exclude_none=True` produces.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_version: Option<String>,
    pub os: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_emulator: Option<String>,
    /// Whether the session runs a system prompt other than the schema default.
    /// Never optional, so it is posted even when false.
    pub custom_system_prompt: bool,
    #[serde(
        rename = "organizationId",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub organization_id: Option<String>,
}

/// The experiment a track names. Reference `TrackedExperiment`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackedExperiment {
    pub key: String,
}

/// What the proxy resolved for one experiment. Reference
/// `TrackedExperimentResult`.
///
/// `in_experiment` is the field that separates an enrollment from a force: the
/// proxy sets it when the user was actually hashed into a variation, and leaves
/// it false or absent for a forced value, a coverage exclusion or a default.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TrackedExperimentResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(
        rename = "variationId",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub variation_id: Option<i64>,
    #[serde(default, skip_serializing_if = "JsonValue::is_null")]
    pub value: JsonValue,
    #[serde(
        rename = "inExperiment",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub in_experiment: Option<bool>,
    #[serde(
        rename = "hashAttribute",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub hash_attribute: Option<String>,
    #[serde(rename = "hashValue", default, skip_serializing_if = "Option::is_none")]
    pub hash_value: Option<String>,
    #[serde(rename = "featureId", default, skip_serializing_if = "Option::is_none")]
    pub feature_id: Option<String>,
}

/// One exposure the proxy recorded on a rule. Reference `TrackData`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrackData {
    pub experiment: TrackedExperiment,
    pub result: TrackedExperimentResult,
}

/// One rule of a feature.
///
/// In remote evaluation mode the proxy has already run the evaluation, so it
/// rewrites every feature as a single pre-resolved rule carrying `force` and
/// the exposure metadata in `tracks`, scrubbing conditions, coverage and unused
/// variations. A rule that still carries a condition is therefore a rule this
/// client cannot evaluate, and reading it as a rule without a force is what
/// both implementations do with it.
///
/// Reference `FeatureRule`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FeatureRule {
    #[serde(default, skip_serializing_if = "JsonValue::is_null")]
    pub force: JsonValue,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tracks: Vec<TrackData>,
}

/// One feature of the response. Reference `FeatureDefinition`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FeatureDefinition {
    #[serde(rename = "defaultValue", default)]
    pub default_value: JsonValue,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<FeatureRule>,
}

impl FeatureDefinition {
    /// The value this feature resolves to: the first rule that forces one, and
    /// otherwise the default.
    ///
    /// A rule forcing `null` is indistinguishable from a rule carrying no force
    /// at all, upstream as here, because the reference's `force: Any = None`
    /// collapses the two.
    ///
    /// Reference `FeatureDefinition.resolved_value`.
    #[must_use]
    pub fn resolved_value(&self) -> &JsonValue {
        self.rules
            .iter()
            .map(|rule| &rule.force)
            .find(|force| !force.is_null())
            .unwrap_or(&self.default_value)
    }
}

/// What the eval endpoint answers.
///
/// The proxy also returns a scrubbed `experiments` block beside `features`, and
/// the reference models neither it nor anything else the payload carries. The
/// unknown keys are ignored here for the same reason.
///
/// Reference `EvalResponse`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EvalResponse {
    #[serde(default)]
    pub features: OrderedMap<FeatureDefinition>,
}
