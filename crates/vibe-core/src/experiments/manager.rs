//! What a resolved eval response means, and the one distinction the whole
//! rollout analysis rests on.
//!
//! A response reaches this manager already resolved: the proxy did the
//! bucketing and rewrote each feature as a pre-resolved rule. What is left to
//! decide is local, and one decision matters more than the rest.
//! [`ExperimentManager::config_variants`] and
//! [`ExperimentManager::assignments`] read the same response and deliberately
//! disagree. Configuration honors a forced value, because a force is how a
//! rollout pins a build to a variant. Telemetry reports only a confirmed
//! exposure, because a force is not an enrollment and counting it as one would
//! corrupt the experiment's own analysis.
//!
//! Reference: `vibe/core/experiments/manager.py` at the pinned commit.

use std::collections::BTreeMap;

use sha2::{Digest, Sha256};

use crate::text::hex_encode;

use super::ExperimentName;
use super::client::RemoteEvalClient;
use super::json::JsonValue;
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
}

impl ExperimentManager {
    #[must_use]
    pub fn new(client: RemoteEvalClient) -> Self {
        Self {
            client,
            response: None,
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
        if let Some(response) = self.client.evaluate(attributes).await {
            self.response = Some(filter_to_known_experiments(response));
        }
    }

    /// Takes a state in without a lookup, which is what a resumed or forked
    /// session does. Replaces whatever was there rather than merging into it.
    ///
    /// Reference `ExperimentManager.hydrate`.
    pub fn hydrate(&mut self, response: EvalResponse) {
        self.response = Some(filter_to_known_experiments(response));
    }

    /// What this manager would hand a session to persist.
    ///
    /// Reference `ExperimentManager.export_state`.
    #[must_use]
    pub fn export_state(&self) -> Option<&EvalResponse> {
        self.response.as_ref()
    }

    /// The resolved value of one experiment, or [`None`] when the response
    /// carries nothing usable for it.
    ///
    /// An object- or array-valued feature answers its JSON serialization
    /// rather than falling back to the default, so a caller receives the real
    /// payload: the routing experiment carries a whole model definition this
    /// way.
    ///
    /// Reference `ExperimentManager.get_variant_or_none`.
    #[must_use]
    pub fn variant_or_none(&self, name: ExperimentName) -> Option<String> {
        let feature = self.response.as_ref()?.features.get(name.key())?;
        match feature.resolved_value() {
            JsonValue::Null => None,
            JsonValue::String(text) => Some(text.clone()),
            other => Some(other.python_json()),
        }
    }

    /// The resolved value of one experiment, falling back to this build's own
    /// default.
    ///
    /// Reference `ExperimentManager.get_variant`.
    #[must_use]
    pub fn variant(&self, name: ExperimentName) -> String {
        self.variant_or_none(name)
            .unwrap_or_else(|| name.default_variant().to_owned())
    }

    /// The variants allowed to reach the configuration layers: every confirmed
    /// exposure, plus a forced value for any experiment that has one and was
    /// not confirmed.
    ///
    /// Reference `ExperimentManager.config_variants`.
    #[must_use]
    pub fn config_variants(&self) -> BTreeMap<String, String> {
        let mut result = self.assignments();
        let Some(response) = self.response.as_ref() else {
            return result;
        };
        for name in ExperimentName::ALL {
            if result.contains_key(name.key()) {
                continue;
            }
            let Some(feature) = response.features.get(name.key()) else {
                continue;
            };
            if let Some(variant) = forced_variant_or_none(feature) {
                result.insert(name.key().to_owned(), variant);
            }
        }
        result
    }

    /// The confirmed exposures, and nothing else.
    ///
    /// A track the proxy did not mark `inExperiment` is skipped, and so is one
    /// whose label bottoms out empty, so telemetry never reports an enrollment
    /// that did not happen or one it cannot name.
    ///
    /// Reference `ExperimentManager.assignments`.
    #[must_use]
    pub fn assignments(&self) -> BTreeMap<String, String> {
        let mut result = BTreeMap::new();
        let Some(response) = self.response.as_ref() else {
            return result;
        };
        for (key, feature) in response.features.iter() {
            for track in feature.rules.iter().flat_map(|rule| &rule.tracks) {
                if track.result.in_experiment != Some(true) {
                    continue;
                }
                let label = variant_label(feature, track);
                if !label.is_empty() {
                    result.insert(key.to_owned(), label);
                }
            }
        }
        result
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
/// Reference `ExperimentManager._filter_to_known_experiments`.
fn filter_to_known_experiments(mut response: EvalResponse) -> EvalResponse {
    response
        .features
        .retain(|key, _| ExperimentName::from_key(key).is_some());
    response
}

/// The first value a rule forces, as a string, or [`None`] when no rule forces
/// one.
///
/// Reference `ExperimentManager._forced_variant_or_none`.
fn forced_variant_or_none(feature: &FeatureDefinition) -> Option<String> {
    feature
        .rules
        .iter()
        .map(|rule| &rule.force)
        .find(|force| !force.is_null())
        .map(|force| match force {
            JsonValue::String(text) => text.clone(),
            other => other.python_json(),
        })
}

/// How one confirmed exposure is named, in the order the reference tries: the
/// value the track carried, then the value the feature resolves to, then the
/// key of the result, then its variation number. An exhausted fallback is the
/// empty string, which the caller reads as "not reportable".
///
/// Reference `ExperimentManager._variant_label`.
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
