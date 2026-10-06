//! What a resolved eval response means, and the one distinction the whole
//! rollout analysis rests on.
//!
//! A response reaches this manager already resolved: the proxy did the
//! bucketing and rewrote each feature as a pre-resolved rule. What is left to
//! decide is local, and one decision matters more than the rest.
//! [`ExperimentManager::config_variants`] and
//! [`ExperimentManager::assignments`] read the same response and deliberately
//! disagree. Configuration honors whatever value resolved, because a force or
//! a feature default is how a rollout pins a build to a variant. Telemetry
//! reports only a confirmed exposure, because a force is not an enrollment and
//! counting it as one would corrupt the experiment's own analysis.
//!
//! Reference: `vibe/core/experiments/manager.py` and
//! `vibe/core/experiments/resolve.py` at the pinned commit.

use sha2::{Digest, Sha256};

use crate::telemetry::ExperimentAssignment;
use crate::text::hex_encode;

use super::ExperimentName;
use super::client::RemoteEvalClient;
use super::json::{JsonValue, OrderedMap};
use super::models::{EvalResponse, ExperimentAttributes, FeatureDefinition, TrackData};

/// How many hexadecimal characters of the digest the bucketing key keeps.
pub const BUCKETING_KEY_LENGTH: usize = 32;

/// The anonymous bucketing key one API key produces.
///
/// The GrowthBook hash attribute has to be stable per user, and it must not be
/// the credential. A SHA-256 truncated to 32 hexadecimal characters is both:
/// stable across calls and processes, and irreversible. This is the only
/// derivative of the API key that ever leaves this process.
///
/// Reference `hash_api_key`.
#[must_use]
pub fn hash_api_key(api_key: &str) -> String {
    let digest = Sha256::digest(api_key.as_bytes());
    let mut encoded = hex_encode(&digest);
    encoded.truncate(BUCKETING_KEY_LENGTH);
    encoded
}

/// The rollout state of one session.
///
/// Reference `ExperimentManager`.
#[derive(Debug)]
pub struct ExperimentManager {
    client: RemoteEvalClient,
    /// What the last successful lookup or hydration resolved, filtered to the
    /// experiments this build knows.
    response: Option<EvalResponse>,
    /// The snapshot the last lookup posted, or the one set without a lookup,
    /// which telemetry reports beside the exposures it segmented.
    attributes: Option<ExperimentAttributes>,
}

impl ExperimentManager {
    #[must_use]
    pub fn new(client: RemoteEvalClient) -> Self {
        Self {
            client,
            response: None,
            attributes: None,
        }
    }

    /// Looks the rollout up and takes the answer in, or leaves the manager
    /// exactly as it was.
    ///
    /// A failed lookup is not a partial one: the previous state stands
    /// untouched, so a refresh that fails cannot empty a session that had
    /// already resolved.
    ///
    /// Reference `ExperimentManager.initialize`.
    pub async fn initialize(&mut self, attributes: &ExperimentAttributes) {
        // Retained before the lookup, so a failed one still reports the
        // snapshot it posted.
        self.attributes = Some(attributes.clone());
        if let Some(response) = self.client.evaluate(attributes).await {
            self.response = Some(filter_to_known_experiments(response));
        }
    }

    /// Takes a state in without a lookup, which is what a resumed or forked
    /// session does. Replaces whatever was there rather than merging into it,
    /// the snapshot included: a hydrated session has none until its plan
    /// attributes are resolved again.
    ///
    /// Reference `ExperimentManager.hydrate`.
    pub fn hydrate(&mut self, response: EvalResponse) {
        self.attributes = None;
        self.response = Some(filter_to_known_experiments(response));
    }

    /// The snapshot the last lookup posted. Reference
    /// `ExperimentManager.attributes`.
    #[must_use]
    pub fn attributes(&self) -> Option<&ExperimentAttributes> {
        self.attributes.as_ref()
    }

    /// Sets the snapshot without a lookup, which is what a session with
    /// nothing to evaluate still reports. Reference
    /// `ExperimentManager.set_attributes`.
    pub fn set_attributes(&mut self, attributes: ExperimentAttributes) {
        self.attributes = Some(attributes);
    }

    /// What this manager would hand a session to persist.
    ///
    /// Reference `ExperimentManager.export_state`.
    #[must_use]
    pub fn export_state(&self) -> Option<&EvalResponse> {
        self.response.as_ref()
    }

    /// The resolved value of one experiment, or [`None`] when the response
    /// carries nothing for it: the value of the first rule that forces one,
    /// else the feature's default, typed as the payload carried it.
    ///
    /// Reference `resolve.variant_or_none`.
    #[must_use]
    pub fn variant_or_none(&self, name: ExperimentName) -> Option<JsonValue> {
        let feature = self.response.as_ref()?.features.get(name.key())?;
        let value = feature.resolved_value();
        (!value.is_null()).then(|| value.clone())
    }

    /// The resolved value of one experiment, falling back to this build's own
    /// default.
    ///
    /// Reference `resolve.variant`.
    #[must_use]
    pub fn variant(&self, name: ExperimentName) -> JsonValue {
        self.variant_or_none(name)
            .unwrap_or_else(|| name.default_variant())
    }

    /// The variants allowed to reach the configuration layers, by feature key
    /// in the order the names are declared: every known experiment whose
    /// resolved value differs from its declared default.
    ///
    /// A forced value and a feature default reach the layer alike, because
    /// the proxy already resolved which applies; a confirmed exposure adds
    /// nothing a resolved value did not already say. A value equal to the
    /// default is dropped, so the low-precedence layer only ever expresses a
    /// deviation from the schema.
    ///
    /// Reference `resolve.config_variants`.
    #[must_use]
    pub fn config_variants(&self) -> OrderedMap<JsonValue> {
        ExperimentName::ALL
            .into_iter()
            .filter_map(|name| {
                let value = self.variant_or_none(name)?;
                (!value.python_eq(&name.default_variant())).then(|| (name.key().to_owned(), value))
            })
            .collect()
    }

    /// The confirmed exposures as the records a census carries: at most one
    /// per experiment, the last confirmed track winning, in the order the
    /// response lists its features.
    ///
    /// A track the proxy did not mark `inExperiment` is skipped, and so is one
    /// whose label bottoms out empty, so telemetry never reports an enrollment
    /// that did not happen or one it cannot name.
    ///
    /// Reference `resolve.assignments`.
    #[must_use]
    pub fn assignments(&self) -> Vec<ExperimentAssignment> {
        let mut records: Vec<ExperimentAssignment> = Vec::new();
        let Some(response) = self.response.as_ref() else {
            return records;
        };
        for (key, feature) in response.features.iter() {
            for track in feature.rules.iter().flat_map(|rule| &rule.tracks) {
                if track.result.in_experiment != Some(true) {
                    continue;
                }
                let label = variant_label(feature, track);
                if label.is_empty() {
                    continue;
                }
                let record = ExperimentAssignment {
                    experiment_id: key.to_owned(),
                    experiment_name: track.experiment.key.clone(),
                    variation_name: label,
                    variation_id: track.result.variation_id,
                    in_experiment: track.result.in_experiment,
                    hash_attribute: track.result.hash_attribute.clone(),
                    hash_value: track.result.hash_value.clone(),
                    feature_id: track.result.feature_id.clone(),
                };
                match records
                    .iter_mut()
                    .find(|existing| existing.experiment_id == record.experiment_id)
                {
                    Some(existing) => *existing = record,
                    None => records.push(record),
                }
            }
        }
        records
    }

    /// Releases the client's transport.
    ///
    /// Reference `ExperimentManager.aclose`.
    pub async fn close(&self) {
        self.client.close().await;
    }
}

/// The response with every feature this build does not know dropped.
///
/// A rollout defined for a newer build reaches this one as an unknown key, and
/// dropping it here is what keeps it out of configuration and out of telemetry
/// alike. Applied on hydration as well as on initialization, so a session
/// written by a newer client cannot smuggle one back in.
///
/// Reference `resolve.filter_to_known`.
fn filter_to_known_experiments(mut response: EvalResponse) -> EvalResponse {
    response
        .features
        .retain(|key, _| ExperimentName::from_key(key).is_some());
    response
}

/// How one confirmed exposure is named, in the order the reference tries: the
/// value the track carried, then the value the feature resolves to, then the
/// key of the result, then its variation number. An exhausted fallback is the
/// empty string, which the caller reads as "not reportable".
///
/// Reference `resolve._variant_label`.
fn variant_label(feature: &FeatureDefinition, track: &TrackData) -> String {
    if let Some(label) = label_of(&track.result.value) {
        return label;
    }
    if let Some(label) = label_of(feature.resolved_value()) {
        return label;
    }
    if let Some(key) = track.result.key.as_ref() {
        return key.clone();
    }
    track
        .result
        .variation_id
        .map(|variation| variation.to_string())
        .unwrap_or_default()
}

/// One JSON value as a label, or [`None`] when it carries nothing.
fn label_of(value: &JsonValue) -> Option<String> {
    match value {
        JsonValue::Null => None,
        JsonValue::String(text) => Some(text.clone()),
        other => Some(other.python_json()),
    }
}
