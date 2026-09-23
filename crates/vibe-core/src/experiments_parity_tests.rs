//! Differential oracle for the experiments engine, its configuration layer and
//! its session gates.
//!
//! `scripts/parity/experiments.py` drives the reference's own
//! `RemoteEvalClient`, `ExperimentManager`, `GrowthbookLayer` and session
//! helpers over inputs the script authors, and records thirteen families into
//! `crates/vibe-core/tests/experiments/corpus.json`. This module replays that
//! corpus against this build unconditionally: only the recapture probe at the
//! bottom skips when the checkout is absent or off-pin.
//!
//! The capture never reaches a network. In remote evaluation mode the GrowthBook
//! proxy performs the bucketing and rewrites every feature as a pre-resolved
//! `force` rule carrying the exposure metadata in `tracks`, so a client has no
//! assignment logic of its own to get wrong. What a client implementation *can*
//! get wrong is entirely local: which URL it builds, what payload it posts, how
//! it fails open, how it resolves a value, which features it keeps, and which
//! variants it lets reach configuration versus telemetry. All of that is
//! measured here by feeding both sides the same synthetic eval response.
//!
//! A key is `family/field/case`, and a trailing `*` covers every key that starts
//! with the prefix. A divergence no entry names fails the replay; an entry whose
//! divergence stopped reproducing fails as stale, which is what forces a row out
//! once the behavior conforms.
//!
//! Every family is compared field by field. Where this build has no counterpart
//! at all the port answer is [`None`], which is what makes an entry go stale the
//! moment the surface starts answering: EP-002 through EP-005 remove these rows
//! by filling [`port_answer`], not by editing the ledger.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use serde_json::{Map, Value};
use tokio::runtime::Runtime;
use toml::Table;

use crate::config::registry::default_document;
use crate::config::{
    ConfigPaths, ConfigTarget, EXPERIMENTS_LAYER_NAME, ExperimentsLayer, LayeredConfig,
    configured_fields, default_model_alias,
};
use crate::experiments::recorder::{Outcome, RecordingSink, RecordingTransport};
use crate::experiments::{
    BUCKETING_KEY_LENGTH, EVAL_PATH_TEMPLATE, EVAL_REQUEST_TIMEOUT, EXPERIMENT_IDENTITY_TIMEOUT,
    EvalPayload, EvalResponse, ExperimentAttributes, ExperimentManager, ExperimentName,
    FeatureDefinition, JsonValue, RemoteEvalClient, build_attributes, build_eval_url, hash_api_key,
    hydrate_experiments_from_session, initialize_experiments,
};
use crate::identity::IDENTITY_PATH;
use crate::identity::recorder::RecordingResolver;
use crate::parity::{REFERENCE_COMMIT, RESTORE_COMMAND, off_pin_reason, reference_root};
use crate::prompt::PromptResolver;
use crate::telemetry::{LaunchContext, platform_id, version};

const CORPUS_RELATIVE: &str = "crates/vibe-core/tests/experiments/corpus.json";
const CAPTURE_SCRIPT: &str = "scripts/parity/experiments.py";
/// The corpus layout this runner reads, matching `SCHEMA_VERSION` in the capture
/// script.
const CORPUS_SCHEMA_VERSION: u32 = 1;
/// The comparison floor this replay commits to, so a regeneration that captured
/// almost nothing fails instead of reporting a clean but empty run.
const MINIMUM_COMPARISONS: usize = 480;

/// Keys the corpus carries that are not families: the pin, the layout and the
/// prose-free note.
const METADATA: [&str; 3] = ["schemaVersion", "reference", "note"];

/// Every family the corpus declares, with the case fields that are *inputs* the
/// capture authored and the case fields that are *answers* both sides give.
///
/// A family the capture adds without an entry here fails the replay by name
/// rather than passing unread, and so does a field: the runner asserts that
/// inputs and answers together account for every key a case carries.
const FAMILIES: &[Family] = &[
    Family {
        name: "constants",
        inputs: &[],
        answers: &["value"],
        toml_document: false,
    },
    Family {
        name: "bucketingKey",
        inputs: &["variable"],
        answers: &["bucketingKey", "length", "stable", "hexadecimal"],
        toml_document: false,
    },
    Family {
        name: "evalUrl",
        inputs: &["apiHost", "clientKey"],
        answers: &["url"],
        toml_document: false,
    },
    Family {
        name: "evalRequest",
        inputs: &[],
        answers: &[
            "method",
            "url",
            "headerNames",
            "payloadKeys",
            "attributeKeys",
            "attributes",
            "forcedVariations",
            "forcedFeatures",
            "urlField",
            "credentialVariable",
            "returnedState",
            "requests",
        ],
        toml_document: false,
    },
    Family {
        name: "evalFailures",
        inputs: &[],
        answers: &[
            "state",
            "requests",
            "variants",
            "variantsAreDefaults",
            "assignments",
            "configVariants",
            "logs",
        ],
        toml_document: false,
    },
    Family {
        name: "featureResolution",
        inputs: &["definition"],
        answers: &["resolved"],
        toml_document: false,
    },
    Family {
        name: "variantResolution",
        inputs: &["response"],
        answers: &["knownFeatures", "variants", "variantsOrNone"],
        toml_document: false,
    },
    Family {
        name: "configVariants",
        inputs: &["response"],
        answers: &["assignments", "configVariants"],
        toml_document: false,
    },
    Family {
        name: "variantLabels",
        inputs: &["definition"],
        answers: &["assignments", "reported"],
        toml_document: false,
    },
    Family {
        name: "configMapping",
        inputs: &["variants"],
        answers: &["data", "hasFingerprint"],
        toml_document: false,
    },
    Family {
        name: "layerPrecedence",
        inputs: &["user", "project", "environment", "overrides", "variants"],
        answers: &["effective"],
        toml_document: true,
    },
    Family {
        name: "sessionGates",
        inputs: &["configuration", "helper"],
        answers: &[
            "returned",
            "evalRequests",
            "identityRequests",
            "identityTimeout",
            "persisted",
            "organizationId",
        ],
        toml_document: false,
    },
    Family {
        name: "attributes",
        inputs: &["document", "context"],
        answers: &["attributes", "payloadKeys", "credentialVariable"],
        toml_document: false,
    },
];

/// Cases where this build answers something other than the reference, each
/// with the reason: the reference change, where it lives at the pin, and what
/// this build does instead.
///
/// Every entry was recorded when the pin moved from b78b451 (v2.24.0) to
/// 4a96003 (v2.25.7): the families all replayed conforming at the old pin, and
/// what these rows measure is what the reference changed since. One entry per
/// case and field, so a row goes stale on its own the moment this build
/// answers it.
const DIVERGENCES: &[(&str, &str)] = &[
    ("constants/value/experimentNames", EXPERIMENT_NAMES),
    ("constants/value/defaultVariants", TYPED_DEFAULTS),
    ("constants/value/identityTimeoutSeconds", IDENTITY_TIMEOUT),
    ("constants/value/configuredFields", CONFIGURED_FIELDS),
    (
        "evalRequest/attributeKeys/every-attribute",
        POSTED_ATTRIBUTES,
    ),
    ("evalRequest/attributes/every-attribute", POSTED_ATTRIBUTES),
    (
        "evalRequest/attributeKeys/optional-attributes-absent",
        POSTED_ATTRIBUTES,
    ),
    (
        "evalRequest/attributes/optional-attributes-absent",
        POSTED_ATTRIBUTES,
    ),
    (
        "evalRequest/attributeKeys/custom-system-prompt",
        POSTED_ATTRIBUTES,
    ),
    (
        "evalRequest/attributes/custom-system-prompt",
        POSTED_ATTRIBUTES,
    ),
    ("evalFailures/variants/connection-error", TYPED_VARIANTS),
    ("evalFailures/variants/timeout", TYPED_VARIANTS),
    ("evalFailures/variants/status-400", TYPED_VARIANTS),
    ("evalFailures/variants/status-404", TYPED_VARIANTS),
    ("evalFailures/variants/status-500", TYPED_VARIANTS),
    ("evalFailures/variants/status-503", TYPED_VARIANTS),
    ("evalFailures/variants/non-json-body", TYPED_VARIANTS),
    (
        "evalFailures/variants/body-fails-validation",
        TYPED_VARIANTS,
    ),
    ("evalFailures/variants/url-unset", TYPED_VARIANTS),
    ("variantResolution/variants/uninitialized", TYPED_VARIANTS),
    (
        "variantResolution/variantsOrNone/uninitialized",
        TYPED_VARIANTS,
    ),
    ("variantResolution/variants/empty-features", TYPED_VARIANTS),
    (
        "variantResolution/variantsOrNone/empty-features",
        TYPED_VARIANTS,
    ),
    ("variantResolution/variants/string-value", TYPED_VARIANTS),
    (
        "variantResolution/variantsOrNone/string-value",
        TYPED_VARIANTS,
    ),
    ("variantResolution/variants/object-value", TYPED_VARIANTS),
    (
        "variantResolution/variantsOrNone/object-value",
        TYPED_VARIANTS,
    ),
    ("variantResolution/variants/array-value", TYPED_VARIANTS),
    (
        "variantResolution/variantsOrNone/array-value",
        TYPED_VARIANTS,
    ),
    ("variantResolution/variants/numeric-value", TYPED_VARIANTS),
    (
        "variantResolution/variantsOrNone/numeric-value",
        TYPED_VARIANTS,
    ),
    ("variantResolution/variants/boolean-value", TYPED_VARIANTS),
    (
        "variantResolution/variantsOrNone/boolean-value",
        TYPED_VARIANTS,
    ),
    ("variantResolution/variants/resolved-null", TYPED_VARIANTS),
    (
        "variantResolution/variantsOrNone/resolved-null",
        TYPED_VARIANTS,
    ),
    (
        "variantResolution/variants/default-value-without-a-force",
        TYPED_VARIANTS,
    ),
    (
        "variantResolution/variantsOrNone/default-value-without-a-force",
        TYPED_VARIANTS,
    ),
    (
        "variantResolution/variants/unknown-feature-key",
        TYPED_VARIANTS,
    ),
    (
        "variantResolution/variantsOrNone/unknown-feature-key",
        TYPED_VARIANTS,
    ),
    (
        "variantResolution/variants/every-known-feature",
        TYPED_VARIANTS,
    ),
    (
        "variantResolution/variantsOrNone/every-known-feature",
        TYPED_VARIANTS,
    ),
    (
        "configVariants/assignments/confirmed-exposure",
        ASSIGNMENT_RECORDS,
    ),
    (
        "configVariants/assignments/default-value-without-a-force",
        ASSIGNMENT_RECORDS,
    ),
    (
        "configVariants/configVariants/default-value-without-a-force",
        CONFIG_VARIANTS_DROP_DEFAULT,
    ),
    (
        "configVariants/configVariants/forced-object-without-tracks",
        CONFIG_VARIANTS_TYPED,
    ),
    (
        "configVariants/assignments/experiment-key-differs-from-the-feature-key",
        ASSIGNMENT_RECORDS,
    ),
    (
        "configVariants/assignments/mixed-confirmed-and-forced",
        ASSIGNMENT_RECORDS,
    ),
    (
        "configVariants/configVariants/mixed-confirmed-and-forced",
        CONFIG_VARIANTS_EVERY_RESOLVED,
    ),
    (
        "variantLabels/assignments/track-value-string",
        ASSIGNMENT_RECORDS,
    ),
    (
        "variantLabels/assignments/track-value-object",
        ASSIGNMENT_RECORDS,
    ),
    (
        "variantLabels/assignments/falls-back-to-the-resolved-value",
        ASSIGNMENT_RECORDS,
    ),
    (
        "variantLabels/assignments/falls-back-to-the-default-value",
        ASSIGNMENT_RECORDS,
    ),
    (
        "variantLabels/assignments/falls-back-to-the-result-key",
        ASSIGNMENT_RECORDS,
    ),
    (
        "variantLabels/assignments/falls-back-to-the-variation-id",
        ASSIGNMENT_RECORDS,
    ),
    (
        "variantLabels/assignments/variation-id-zero",
        ASSIGNMENT_RECORDS,
    ),
    (
        "variantLabels/assignments/last-confirmed-track-wins",
        ASSIGNMENT_RECORDS,
    ),
    (
        "configMapping/data/routing-with-model-config",
        ROUTED_MODELS_MAP,
    ),
    ("configMapping/data/every-experiment", ROUTED_MODELS_MAP),
    (
        "layerPrecedence/effective/routing-variant-loses-to-a-pinned-model",
        PINNED_ROUTING,
    ),
    (
        "sessionGates/identityTimeout/initialize/mistral-active/identity",
        IDENTITY_TIMEOUT,
    ),
    (
        "sessionGates/identityTimeout/initialize/mistral-active/no-identity",
        IDENTITY_TIMEOUT,
    ),
    (
        "sessionGates/identityRequests/initialize/experiments-disabled/identity",
        IDENTITY_BEFORE_EXPERIMENTS_GATE,
    ),
    (
        "sessionGates/identityTimeout/initialize/experiments-disabled/identity",
        IDENTITY_BEFORE_EXPERIMENTS_GATE,
    ),
    (
        "sessionGates/identityRequests/initialize/experiments-disabled/no-identity",
        IDENTITY_BEFORE_EXPERIMENTS_GATE,
    ),
    (
        "sessionGates/identityTimeout/initialize/experiments-disabled/no-identity",
        IDENTITY_BEFORE_EXPERIMENTS_GATE,
    ),
    (
        "sessionGates/returned/initialize/third-party-only/identity",
        NO_PLAN_SENTINEL,
    ),
    (
        "sessionGates/returned/initialize/third-party-only/no-identity",
        NO_PLAN_SENTINEL,
    ),
    (
        "sessionGates/identityTimeout/initialize/custom-system-prompt/identity",
        IDENTITY_TIMEOUT,
    ),
    (
        "sessionGates/identityTimeout/initialize/custom-system-prompt/no-identity",
        IDENTITY_TIMEOUT,
    ),
    (
        "sessionGates/identityTimeout/initialize/eval-fails",
        IDENTITY_TIMEOUT,
    ),
    (
        "sessionGates/identityTimeout/initialize/eval-returns-nothing",
        IDENTITY_TIMEOUT,
    ),
    (
        "attributes/attributes/default-prompt/no-launch-context",
        BUILT_ATTRIBUTES,
    ),
    (
        "attributes/payloadKeys/default-prompt/no-launch-context",
        POSTED_ATTRIBUTE_KEYS,
    ),
    ("attributes/attributes/default-prompt/cli", BUILT_ATTRIBUTES),
    (
        "attributes/payloadKeys/default-prompt/cli",
        POSTED_ATTRIBUTE_KEYS,
    ),
    (
        "attributes/attributes/default-prompt/cli-in-vscode",
        BUILT_ATTRIBUTES,
    ),
    (
        "attributes/payloadKeys/default-prompt/cli-in-vscode",
        POSTED_ATTRIBUTE_KEYS,
    ),
    (
        "attributes/attributes/default-prompt/acp-in-cursor",
        BUILT_ATTRIBUTES,
    ),
    (
        "attributes/payloadKeys/default-prompt/acp-in-cursor",
        POSTED_ATTRIBUTE_KEYS,
    ),
    (
        "attributes/attributes/default-prompt/programmatic",
        BUILT_ATTRIBUTES,
    ),
    (
        "attributes/payloadKeys/default-prompt/programmatic",
        POSTED_ATTRIBUTE_KEYS,
    ),
    (
        "attributes/attributes/custom-prompt/no-launch-context",
        BUILT_ATTRIBUTES,
    ),
    (
        "attributes/payloadKeys/custom-prompt/no-launch-context",
        POSTED_ATTRIBUTE_KEYS,
    ),
    ("attributes/attributes/custom-prompt/cli", BUILT_ATTRIBUTES),
    (
        "attributes/payloadKeys/custom-prompt/cli",
        POSTED_ATTRIBUTE_KEYS,
    ),
    (
        "attributes/attributes/custom-prompt/cli-in-vscode",
        BUILT_ATTRIBUTES,
    ),
    (
        "attributes/payloadKeys/custom-prompt/cli-in-vscode",
        POSTED_ATTRIBUTE_KEYS,
    ),
    (
        "attributes/attributes/custom-prompt/acp-in-cursor",
        BUILT_ATTRIBUTES,
    ),
    (
        "attributes/payloadKeys/custom-prompt/acp-in-cursor",
        POSTED_ATTRIBUTE_KEYS,
    ),
    (
        "attributes/attributes/custom-prompt/programmatic",
        BUILT_ATTRIBUTES,
    ),
    (
        "attributes/payloadKeys/custom-prompt/programmatic",
        POSTED_ATTRIBUTE_KEYS,
    ),
];

const EXPERIMENT_NAMES: &str = "v2.24.5, v2.25.0 and v2.25.1 added five experiment names (vibe/core/experiments/active.py:14-27 at 4a96003): vibe_cli_extra_models, vibe_cli_registry_skills, vibe_cli_smart_approve, vibe_cli_smart_approve_default and vibe_cli_unified_harness_rollout. This build's ExperimentName::ALL still declares the three names of v2.24.0 (crates/vibe-core/src/experiments.rs:94).";

const TYPED_DEFAULTS: &str = "v2.25.1 typed DEFAULT_VARIANTS (vibe/core/experiments/active.py:30-39 at 4a96003): the routing default is the object {} rather than the text \"{}\", and the five names added since v2.24.5 default to false, {} or \"legacy\". This build's default_variant answers text for its three names only (crates/vibe-core/src/experiments.rs:119-127).";

const IDENTITY_TIMEOUT: &str = "v2.24.4 raised EXPERIMENT_IDENTITY_TIMEOUT_S from 4.0 to 10.0 seconds (vibe/core/experiments/session.py:28 at 4a96003). This build still bounds the identity lookup at 4 seconds (crates/vibe-core/src/experiments/session.rs:37).";

const CONFIGURED_FIELDS: &str = "v2.24.5, v2.25.0 and v2.25.1 added four GrowthBook mappings (vibe/core/config/layers/growthbook.py:88-116 at 4a96003): vibe_cli_extra_models to routed_extra_models, vibe_cli_smart_approve to smart_approve_available, vibe_cli_smart_approve_default to smart_approve_default, and vibe_cli_registry_skills to experimental_enable_registry_skills. This build maps its three experiments only (crates/vibe-core/src/config/experiments_layer.rs:41-47).";

const POSTED_ATTRIBUTES: &str = "v2.25.0 added the required harness attribute and the arch attribute, which defaults to the host's lowercased machine name (vibe/core/experiments/models.py:27,32 at 4a96003); the capture posts the legacy surface and records arch as a placeholder. This build's ExperimentAttributes carries neither and posts only its nine v2.24.0 keys (crates/vibe-core/src/experiments/models.rs:36-59).";

const TYPED_VARIANTS: &str = "v2.25.1 made variant resolution typed (vibe/core/experiments/resolve.py:27-39 at 4a96003) and v2.24.5 to v2.25.1 added five names (vibe/core/experiments/active.py:14-39): get_variant and get_variant_or_none answer the JSON value itself for eight names. This build answers text, JSON-encoding a non-string value, for its three names (crates/vibe-core/src/experiments/manager.rs:106-123).";

const ASSIGNMENT_RECORDS: &str = "v2.24.3 turned assignments() into ExperimentAssignment records (vibe/core/experiments/resolve.py:59-86 and vibe/core/telemetry/types.py:49-60 at 4a96003) that carry experiment_name, variation_id, in_experiment, hash_attribute, hash_value and feature_id beside the feature key and the label. This build answers a map from feature key to label (crates/vibe-core/src/experiments/manager.rs:158), so only experiment_id and variation_name have a counterpart.";

const CONFIG_VARIANTS_DROP_DEFAULT: &str = "v2.25.1 rewrote config_variants (vibe/core/experiments/resolve.py:42-56 at 4a96003): it keeps each known name's resolved value unless it equals the typed default, and no longer adds the confirmed assignment labels, so a defaultValue of \"cli\" reaches no layer. This build still unions the assignment labels in (crates/vibe-core/src/experiments/manager.rs:125-148) and passes the label \"cli\" on.";

const CONFIG_VARIANTS_TYPED: &str = "v2.25.1 made config_variants typed (vibe/core/experiments/resolve.py:42-56 at 4a96003): a forced object reaches the layer as that object. This build passes its JSON text on (crates/vibe-core/src/experiments/manager.rs:125-148).";

const CONFIG_VARIANTS_EVERY_RESOLVED: &str = "Under the v2.25.1 rule (vibe/core/experiments/resolve.py:42-56 at 4a96003) every known name whose resolved value differs from its typed default reaches the layer: the routing feature's defaultValue, the text \"{}\", differs from the default object {} (vibe/core/experiments/active.py:33), so the reference passes it on. This build lets only a confirmed label or a forced value through (crates/vibe-core/src/experiments/manager.rs:125-148) and drops the unforced routing feature.";

const ROUTED_MODELS_MAP: &str = "v2.25.0 made the GrowthBook layer also write a models table keyed by alias for every routed model definition that validates (vibe/core/config/layers/growthbook.py:147-153,166-183 at 4a96003). This build's ExperimentsLayer writes only the mapped fields (crates/vibe-core/src/config/experiments_layer.rs:79-99).";

const PINNED_ROUTING: &str = "v2.24.1 relaxed the unpinned-only guard in _inject_routed_model to also inject when active_model equals the routed alias, and v2.24.2 removed the guard entirely (vibe/core/config/vibe_schema.py:836-855 at 4a96003, against vibe/core/config/vibe_schema.py:604-617 at b78b451), and v2.25.0 also routes the definition through the layer's models table (vibe/core/config/layers/growthbook.py:147-153), so a pinned installation declares the routed alias and resolves it as the default. This build still skips the injection when active_model is pinned (crates/vibe-core/src/config/effective.rs:255-268) and resolves mistral-medium-3.5.";

const IDENTITY_BEFORE_EXPERIMENTS_GATE: &str = "v2.24.4 reordered initialize_experiments (vibe/core/experiments/session.py:103-130 at 4a96003): only enable_telemetry precedes the identity and whoami lookups, and experiments.enable = false now skips the eval alone, so the identity is still requested once with the 10 second budget. This build checks both gates before any lookup (crates/vibe-core/src/experiments/session.rs:72-74,121-133) and requests nothing.";

const NO_PLAN_SENTINEL: &str = "v2.24.4 made initialize_experiments return (refreshed, user_plan) and answer the NO_PLAN_DATA plan when no Mistral provider is configured (vibe/core/experiments/session.py:64-71,111-131 at 4a96003; vibe/setup/auth/whoami.py:42). This build resolves no plan (crates/vibe-core/src/experiments/session.rs:64-92), which the replay reads as a false refresh beside an absent plan.";

const BUILT_ATTRIBUTES: &str = "v2.24.2 to v2.25.0 reshaped _build_attributes (vibe/core/experiments/session.py:215-257 at 4a96003): userId is the identity's id rather than hash_api_key of the key, harness and arch are set, and organizationKind, workspaceId, customerId, planType and planName are declared (vibe/core/experiments/models.py:23-40). This build still derives userId from the key digest and declares nine fields (crates/vibe-core/src/experiments/session.rs:173-196, crates/vibe-core/src/experiments/models.rs:36-59).";

const POSTED_ATTRIBUTE_KEYS: &str = "v2.25.0 added harness and arch to the posted attributes (vibe/core/experiments/models.py:27,32 at 4a96003), and neither is None for a built attribute set. This build posts neither (crates/vibe-core/src/experiments/models.rs:36-59).";

/// One family's shape: which case fields the capture authored and which ones
/// both sides answer.
struct Family {
    name: &'static str,
    inputs: &'static [&'static str],
    answers: &'static [&'static str],
    /// Whether this family's answers are read out of a TOML document, where a
    /// reference `None` is an absent key rather than an explicit null.
    ///
    /// TOML carries no null, so a reference field validated to `None` has no
    /// counterpart here and the expectation drops it before the comparison.
    /// Same normalization as `config/surface_parity_tests.rs`, which reads the
    /// merged document the same way. A key this port fills where the reference
    /// answered null still diverges, because the answer keeps it.
    toml_document: bool,
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("the crate sits two levels below the repository root")
        .to_path_buf()
}

fn corpus_text() -> String {
    let path = repo_root().join(CORPUS_RELATIVE);
    fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("{} is readable: {error}", path.display()))
}

/// The corpus with every object key in the order the capture wrote it, which is
/// what an input carrying a wire order is read from. See [`Case`].
fn ordered_corpus(raw: &str) -> JsonValue {
    serde_json::from_str(raw).expect("the experiments corpus parses in order")
}

fn corpus() -> Map<String, Value> {
    let raw = corpus_text();
    let parsed: Value = serde_json::from_str(&raw).expect("the experiments corpus parses");
    let corpus = parsed
        .as_object()
        .expect("the experiments corpus is an object")
        .clone();
    assert_eq!(
        corpus.get("schemaVersion").and_then(Value::as_u64),
        Some(u64::from(CORPUS_SCHEMA_VERSION)),
        "the corpus layout moved; regenerate it with {CAPTURE_SCRIPT}"
    );
    assert_eq!(
        corpus
            .get("reference")
            .and_then(|reference| reference.get("commit"))
            .and_then(Value::as_str),
        Some(REFERENCE_COMMIT),
        "the corpus was captured from an unpinned reference"
    );
    corpus
}

/// Every case of one family, each as its object.
fn cases<'a>(corpus: &'a Map<String, Value>, family: &str) -> &'a Vec<Value> {
    corpus
        .get(family)
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("the corpus carries the {family} family as an array"))
}

fn case_id(case: &Value) -> &str {
    case.get("id")
        .and_then(Value::as_str)
        .expect("every corpus case carries an id")
}

// --------------------------------------------------------------------------
// The ledger
// --------------------------------------------------------------------------

/// Whether a ledger entry covers a divergence key: an exact match, or a
/// `prefix*` entry the key starts with.
fn covers(entry: &str, key: &str) -> bool {
    entry
        .strip_suffix('*')
        .map_or(entry == key, |prefix| key.starts_with(prefix))
}

/// Records one comparison, so a family reports a count and a divergence names
/// itself instead of stopping at the first one.
#[derive(Default)]
struct Report {
    conformant: usize,
    total: usize,
    divergences: Vec<String>,
    observed: Vec<String>,
}

impl Report {
    /// One comparison against a surface this build may not have yet. The port
    /// answer is [`None`] where the surface is absent, which is what makes the
    /// ledger entry go stale the moment it starts answering.
    fn check(
        &mut self,
        family: &str,
        field: &str,
        case: &str,
        expected: &Value,
        actual: Option<&Value>,
    ) {
        self.total = self.total.saturating_add(1);
        if actual == Some(expected) {
            self.conformant = self.conformant.saturating_add(1);
            return;
        }
        self.observed.push(format!("{family}/{field}/{case}"));
        self.divergences.push(format!(
            "{family}/{field}/{case}: reference {expected}, port {}",
            actual.map_or_else(|| "absent".to_owned(), Value::to_string)
        ));
    }
}

/// What the ledger has to say about one family's report: the divergences it does
/// not name, and the entries whose divergence no longer reproduces.
fn audit(report: &Report, family: &str, ledger: &[(&str, &str)]) -> (Vec<String>, Vec<String>) {
    let unrecorded = report
        .divergences
        .iter()
        .filter(|line| {
            let key = line.split(':').next().unwrap_or_default();
            !ledger.iter().any(|(entry, _)| covers(entry, key))
        })
        .cloned()
        .collect::<Vec<_>>();
    let family_prefix = format!("{family}/");
    let stale = ledger
        .iter()
        .map(|(entry, _)| (*entry).to_owned())
        .filter(|entry| entry.starts_with(&family_prefix))
        .filter(|entry| !report.observed.iter().any(|key| covers(entry, key)))
        .collect::<Vec<_>>();
    (unrecorded, stale)
}

/// Fails on any divergence the ledger does not name, and on any ledger entry
/// whose divergence no longer reproduces, then reports the family's count.
fn settle(report: &Report, family: &str) -> usize {
    let (unrecorded, stale) = audit(report, family, DIVERGENCES);
    assert!(
        unrecorded.is_empty(),
        "{family} diverges from the reference and is unrecorded:\n{}",
        unrecorded.join("\n")
    );
    assert!(
        stale.is_empty(),
        "these {family} entries conform now and their ledger entry is stale: {stale:?}"
    );
    let ledgered = report.total.saturating_sub(report.conformant);
    println!(
        "experiments: {family} {}/{} conform ({ledgered} ledgered)",
        report.conformant, report.total
    );
    report.total
}

// --------------------------------------------------------------------------
// The inputs the scenarios are driven over
// --------------------------------------------------------------------------

/// The eval host and key every request scenario is built from, spelled the way
/// `scripts/parity/experiments.py` spells them. Neither is a credential: both
/// are authored values, and the GrowthBook client key is publishable by the
/// vendor's own security documentation in any case.
const ORACLE_API_HOST: &str = "https://experiments.example.test";
const ORACLE_CLIENT_KEY: &str = "sdk-oracle-client-key";

/// The userId every request scenario posts, authored rather than resolved
/// from an identity so no scenario needs a credential to build a request.
const ORACLE_USER_ID: &str = "0123456789abcdef0123456789abcdef";

/// What the capture writes in place of the machine's own platform.
const PLATFORM_ID_PLACEHOLDER: &str = "{platformId}";

/// What the capture writes in place of the running build's own version, which
/// is what an attribute answers when no adapter reports one.
const VERSION_PLACEHOLDER: &str = "{version}";

/// The credential sentinels the capture exports, mirroring `SENTINELS` in
/// `scripts/parity/experiments.py`. They are authored strings that stand where
/// an API key would, which is what lets the API-key digest family be measured
/// without one.
const SENTINELS: [(&str, &str); 4] = [
    ("MISTRAL_API_KEY", "oracle-default-sentinel"),
    ("ORACLE_MISTRAL_KEY", "oracle-mistral-sentinel"),
    ("ORACLE_SECOND_KEY", "oracle-second-sentinel"),
    ("ORACLE_THIRD_PARTY_KEY", "oracle-third-party-sentinel"),
];

/// The variable a value is the sentinel of, or [`None`] when it is not one.
///
/// A request payload answering a variable here would mean the key itself had
/// leaked into the request, which is the assertion the `credentialVariable`
/// field carries.
fn sentinel_variable(value: &str) -> Option<&'static str> {
    SENTINELS
        .into_iter()
        .find(|(_, sentinel)| *sentinel == value)
        .map(|(name, _)| name)
}

/// One case, as the corpus object the answers are compared against and as the
/// ordered value its inputs were written in.
///
/// Both readings are needed. The comparison machinery works on
/// [`serde_json::Value`], whose objects are sorted, which is harmless for an
/// answer because a map compares the same either way. An *input* is different:
/// an eval response is re-serialized on the way into this build, and a variant
/// resolving to an object answers the text of that serialization, so the order
/// the capture wrote is part of the question.
struct Case<'a> {
    id: &'a str,
    object: &'a Map<String, Value>,
    ordered: &'a JsonValue,
}

impl Case<'_> {
    /// One input field as the JSON text it was written as, keys in order, or
    /// [`None`] when the field is absent or null.
    fn input_text(&self, field: &str) -> Option<String> {
        let value = self.ordered.as_object()?.get(field)?;
        (!value.is_null()).then(|| value.python_json())
    }

    /// One input field as a string.
    fn input_str(&self, field: &str) -> Option<&str> {
        self.object.get(field).and_then(Value::as_str)
    }
}

// --------------------------------------------------------------------------
// This build's answers
// --------------------------------------------------------------------------

/// Every answer this build gives for one case, or [`None`] where the surface
/// the case measures does not exist here yet.
///
/// This is the single seam every later story widens. An entry the ledger names
/// goes stale as soon as a match here starts answering, so a surface cannot land
/// without the row that recorded its absence coming out in the same change.
fn port_case(family: &str, case: &Case<'_>, runtime: &Runtime) -> Option<Map<String, Value>> {
    match family {
        "constants" => constants_answer(case.id),
        "bucketingKey" => Some(bucketing_key_answer(case)),
        "evalUrl" => Some(eval_url_answer(case)),
        "evalRequest" => Some(eval_request_answer(case, runtime)),
        "evalFailures" => Some(eval_failures_answer(case, runtime)),
        "featureResolution" => Some(feature_resolution_answer(case)),
        "variantResolution" => Some(variant_resolution_answer(case)),
        "configVariants" => Some(config_variants_answer(case)),
        "variantLabels" => Some(variant_labels_answer(case)),
        "configMapping" => Some(config_mapping_answer(case)),
        "layerPrecedence" => Some(layer_precedence_answer(case)),
        "sessionGates" => Some(session_gates_answer(case, runtime)),
        "attributes" => Some(attributes_answer(case)),
        _ => None,
    }
}

/// One answered field, as the map a family builder returns.
fn answered(field: &str, value: Value) -> Map<String, Value> {
    let mut answers = Map::new();
    answers.insert(field.to_owned(), value);
    answers
}

/// A [`JsonValue`] as the value the comparison works on.
fn as_value(value: &JsonValue) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

/// A map keyed by experiment name, which is the shape several families answer.
fn by_experiment<F: Fn(ExperimentName) -> Value>(answer: F) -> Value {
    Value::Object(
        ExperimentName::ALL
            .into_iter()
            .map(|name| (name.key().to_owned(), answer(name)))
            .collect(),
    )
}

// -- constants ------------------------------------------------------------

fn constants_answer(case: &str) -> Option<Map<String, Value>> {
    let value = match case {
        "experimentNames" => Value::Array(
            ExperimentName::ALL
                .into_iter()
                .map(|name| Value::String(name.key().to_owned()))
                .collect(),
        ),
        "defaultVariants" => by_experiment(|name| Value::String(name.default_variant().to_owned())),
        // The reference ties its names to its defaults with a module-level
        // assertion. Here `default_variant` is an exhaustive match, so a name
        // added without a default does not compile and this answer is a
        // property of the type rather than of a runtime check.
        "everyNameHasADefault" => Value::Bool(true),
        "evalPathTemplate" => Value::String(EVAL_PATH_TEMPLATE.to_owned()),
        "identityPath" => Value::String(IDENTITY_PATH.to_owned()),
        "identityTimeoutSeconds" => seconds(EXPERIMENT_IDENTITY_TIMEOUT),
        "evalTimeoutSeconds" => timeout_seconds(),
        "bucketingKeyLength" => Value::from(BUCKETING_KEY_LENGTH),
        "layerName" => Value::String(EXPERIMENTS_LAYER_NAME.to_owned()),
        "configuredFields" => by_experiment(|name| {
            Value::Array(
                configured_fields(name)
                    .iter()
                    .map(|field| Value::String((*field).to_owned()))
                    .collect(),
            )
        }),
        "payloadKeys" => {
            let payload =
                serde_json::to_value(EvalPayload::new(oracle_attributes("every-attribute")))
                    .expect("the payload serializes");
            sorted_keys(&payload)
        }
        // The `[experiments]` table is the socket the engine plugs into, and
        // this port already publishes it with the reference's own schema and
        // defaults, down to the committed client key.
        "experimentsConfigDefaults" => published_experiments_defaults(),
        _ => return None,
    };
    Some(answered("value", value))
}

/// The eval timeout as the number the corpus records, read off the duration the
/// HTTP client is actually built with.
fn timeout_seconds() -> Value {
    seconds(EVAL_REQUEST_TIMEOUT)
}

/// One duration as the number of seconds the corpus records.
fn seconds(duration: std::time::Duration) -> Value {
    serde_json::Number::from_f64(duration.as_secs_f64()).map_or(Value::Null, Value::Number)
}

/// The keys of one JSON object, sorted, which is how the capture records a key
/// set.
fn sorted_keys(value: &Value) -> Value {
    let mut keys = value
        .as_object()
        .map(|object| object.keys().cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    keys.sort();
    Value::Array(keys.into_iter().map(Value::String).collect())
}

/// The `[experiments]` defaults this build's published schema carries, read the
/// way a running binary reads them.
fn published_experiments_defaults() -> Value {
    let table = default_document();
    let experiments = table
        .get("experiments")
        .and_then(toml::Value::as_table)
        .expect("the published document declares the experiments table");
    let mut answered = Map::new();
    for key in ["enable", "api_host", "client_key"] {
        let value = experiments
            .get(key)
            .unwrap_or_else(|| panic!("the experiments table declares {key}"));
        answered.insert(
            key.to_owned(),
            serde_json::to_value(value).expect("a TOML scalar converts to JSON"),
        );
    }
    Value::Object(answered)
}

// -- bucketingKey ---------------------------------------------------------

fn bucketing_key_answer(case: &Case<'_>) -> Map<String, Value> {
    let mut answers = Map::new();
    let Some(variable) = case.input_str("variable") else {
        // The closing case asserts that the three keys hash apart, which is the
        // property that makes the bucketing per user rather than per build.
        let digests = SENTINELS
            .into_iter()
            .filter(|(name, _)| *name != "ORACLE_THIRD_PARTY_KEY")
            .map(|(_, sentinel)| hash_api_key(sentinel))
            .collect::<BTreeSet<_>>();
        answers.insert("bucketingKey".to_owned(), Value::Null);
        answers.insert("length".to_owned(), Value::Null);
        answers.insert("hexadecimal".to_owned(), Value::Null);
        answers.insert("stable".to_owned(), Value::Bool(digests.len() == 3));
        return answers;
    };
    let sentinel = SENTINELS
        .into_iter()
        .find(|(name, _)| *name == variable)
        .map(|(_, sentinel)| sentinel)
        .unwrap_or_else(|| panic!("{variable} is a sentinel this replay knows"));
    let digest = hash_api_key(sentinel);
    answers.insert("length".to_owned(), Value::from(digest.len()));
    answers.insert(
        "stable".to_owned(),
        Value::Bool(digest == hash_api_key(sentinel)),
    );
    answers.insert(
        "hexadecimal".to_owned(),
        Value::Bool(
            digest
                .chars()
                .all(|character| character.is_ascii_digit() || ('a'..='f').contains(&character)),
        ),
    );
    answers.insert("bucketingKey".to_owned(), Value::String(digest));
    answers
}

// -- evalUrl --------------------------------------------------------------

fn eval_url_answer(case: &Case<'_>) -> Map<String, Value> {
    let host = case.input_str("apiHost").unwrap_or_default();
    let key = case.input_str("clientKey").unwrap_or_default();
    answered(
        "url",
        build_eval_url(host, key).map_or(Value::Null, Value::String),
    )
}

// -- evalRequest ----------------------------------------------------------

/// The attributes one request scenario posts.
fn oracle_attributes(case: &str) -> ExperimentAttributes {
    let mut attributes = ExperimentAttributes {
        user_id: ORACLE_USER_ID.to_owned(),
        entrypoint: "cli".to_owned(),
        agent_version: "9.9.9".to_owned(),
        client_name: Some("oracle-client".to_owned()),
        client_version: Some("1.2.3".to_owned()),
        os: platform_id(),
        terminal_emulator: Some("vscode".to_owned()),
        custom_system_prompt: false,
        organization_id: Some("oracle-organization".to_owned()),
    };
    match case {
        "optional-attributes-absent" => {
            attributes.client_name = None;
            attributes.client_version = None;
            attributes.terminal_emulator = None;
            attributes.organization_id = None;
        }
        "custom-system-prompt" => {
            attributes.custom_system_prompt = true;
            attributes.entrypoint = "acp".to_owned();
        }
        _ => {}
    }
    attributes
}

/// The machine's own platform and this build's own version replaced by the
/// placeholders the capture records, so the corpus stays portable across
/// machines and across releases.
fn scrub(value: &Value) -> Value {
    match value {
        Value::String(text) if *text == platform_id() => {
            Value::String(PLATFORM_ID_PLACEHOLDER.to_owned())
        }
        Value::String(text) if text == version() => Value::String(VERSION_PLACEHOLDER.to_owned()),
        Value::Object(entries) => Value::Object(
            entries
                .iter()
                .map(|(key, item)| (key.clone(), scrub(item)))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn eval_request_answer(case: &Case<'_>, runtime: &Runtime) -> Map<String, Value> {
    let mut answers = Map::new();
    let url = build_eval_url(ORACLE_API_HOST, ORACLE_CLIENT_KEY);
    for field in [
        "method",
        "url",
        "headerNames",
        "payloadKeys",
        "attributeKeys",
        "attributes",
        "forcedVariations",
        "forcedFeatures",
        "urlField",
        "credentialVariable",
        "returnedState",
    ] {
        answers.insert(field.to_owned(), Value::Null);
    }
    answers.insert("requests".to_owned(), Value::from(0));

    match case.id {
        // The timeout is a property of the client the reference builds lazily,
        // read here off the duration this port's client is built with. `httpx`
        // budgets each phase and `reqwest` budgets the whole request, so the
        // same number bounds all four phases here and this port is never the
        // more permissive of the two.
        "lazyClientTimeout" => {
            let seconds = timeout_seconds();
            answers.insert("url".to_owned(), url.map_or(Value::Null, Value::String));
            answers.insert(
                "attributes".to_owned(),
                Value::Object(
                    ["connect", "read", "write", "pool"]
                        .into_iter()
                        .map(|phase| (phase.to_owned(), seconds.clone()))
                        .collect(),
                ),
            );
        }
        "closeIsIdempotent" => {
            let transport = Arc::new(RecordingTransport::answering(r#"{"features": {}}"#));
            let client = RemoteEvalClient::with_transport(url, Arc::clone(&transport) as _);
            runtime.block_on(async {
                client.close().await;
                client.close().await;
            });
            answers.insert(
                "attributes".to_owned(),
                Value::Object(
                    [("closes".to_owned(), Value::from(transport.closes()))]
                        .into_iter()
                        .collect(),
                ),
            );
        }
        scenario => {
            let attributes = oracle_attributes(scenario);
            let transport = Arc::new(RecordingTransport::answering(r#"{"features": {}}"#));
            let client = RemoteEvalClient::with_transport(url.clone(), Arc::clone(&transport) as _);
            let state = runtime.block_on(client.evaluate(&attributes));
            let requests = transport.requests();
            let request = requests
                .first()
                .unwrap_or_else(|| panic!("the {scenario} scenario issued its request"));
            let payload = &request.payload;
            answers.insert("method".to_owned(), Value::String(request.method.clone()));
            answers.insert("url".to_owned(), Value::String(request.url.clone()));
            answers.insert(
                "headerNames".to_owned(),
                Value::Array(
                    request
                        .header_names
                        .iter()
                        .map(|name| Value::String(name.clone()))
                        .collect(),
                ),
            );
            answers.insert("payloadKeys".to_owned(), sorted_keys(payload));
            let posted = payload.get("attributes").cloned().unwrap_or(Value::Null);
            answers.insert("attributeKeys".to_owned(), sorted_keys(&posted));
            answers.insert("attributes".to_owned(), scrub(&posted));
            answers.insert(
                "forcedVariations".to_owned(),
                payload
                    .get("forcedVariations")
                    .cloned()
                    .unwrap_or(Value::Null),
            );
            answers.insert(
                "forcedFeatures".to_owned(),
                payload
                    .get("forcedFeatures")
                    .cloned()
                    .unwrap_or(Value::Null),
            );
            answers.insert(
                "urlField".to_owned(),
                payload.get("url").cloned().unwrap_or(Value::Null),
            );
            answers.insert(
                "credentialVariable".to_owned(),
                posted
                    .get("userId")
                    .and_then(Value::as_str)
                    .and_then(sentinel_variable)
                    .map_or(Value::Null, |variable| Value::String(variable.to_owned())),
            );
            answers.insert(
                "returnedState".to_owned(),
                state.map_or(Value::Null, |_| Value::String("features".to_owned())),
            );
            answers.insert(
                "requests".to_owned(),
                Value::from(transport.request_count()),
            );
        }
    }
    answers
}

// -- evalFailures ---------------------------------------------------------

/// What the transport does for one failure scenario, or [`None`] where the
/// scenario has no transport at all because the URL is unset.
fn failure_outcome(case: &str) -> Option<Outcome> {
    let body = r#"{"features": {}}"#;
    match case {
        "connection-error" | "timeout" => Some(Outcome::Failure),
        "status-400" => Some(Outcome::Answer {
            status: 400,
            body: body.to_owned(),
        }),
        "status-404" => Some(Outcome::Answer {
            status: 404,
            body: body.to_owned(),
        }),
        "status-500" => Some(Outcome::Answer {
            status: 500,
            body: body.to_owned(),
        }),
        "status-503" => Some(Outcome::Answer {
            status: 503,
            body: body.to_owned(),
        }),
        "non-json-body" => Some(Outcome::ok("<html>nope</html>")),
        "body-fails-validation" => Some(Outcome::ok(r#"{"features": 17}"#)),
        "url-unset" => None,
        other => panic!("the {other} failure scenario has no script in this replay"),
    }
}

/// A client for one failure scenario, and the recorder standing where its
/// connection would be.
fn failure_client(case: &str) -> (RemoteEvalClient, Option<Arc<RecordingTransport>>) {
    let url = build_eval_url(ORACLE_API_HOST, ORACLE_CLIENT_KEY);
    match failure_outcome(case) {
        Some(outcome) => {
            let transport = Arc::new(RecordingTransport::new(outcome));
            (
                RemoteEvalClient::with_transport(url, Arc::clone(&transport) as _),
                Some(transport),
            )
        }
        None => (RemoteEvalClient::with_url(None), None),
    }
}

fn eval_failures_answer(case: &Case<'_>, runtime: &Runtime) -> Map<String, Value> {
    let attributes = oracle_attributes("every-attribute");
    let (client, transport) = failure_client(case.id);
    let mut manager = ExperimentManager::new(client);
    runtime.block_on(manager.initialize(&attributes));

    let mut answers = Map::new();
    answers.insert(
        "state".to_owned(),
        manager.export_state().map_or(Value::Null, |state| {
            serde_json::to_value(state).unwrap_or(Value::Null)
        }),
    );
    answers.insert(
        "requests".to_owned(),
        Value::from(
            transport
                .as_ref()
                .map_or(0, |recorder| recorder.request_count()),
        ),
    );
    let variants = by_experiment(|name| Value::String(manager.variant(name)));
    let defaults = by_experiment(|name| Value::String(name.default_variant().to_owned()));
    answers.insert(
        "variantsAreDefaults".to_owned(),
        Value::Bool(variants == defaults),
    );
    answers.insert("variants".to_owned(), variants);
    answers.insert(
        "assignments".to_owned(),
        assignment_records(&manager.assignments()),
    );
    answers.insert(
        "configVariants".to_owned(),
        serde_json::to_value(manager.config_variants()).unwrap_or(Value::Null),
    );

    // The same scenario a second time, through the seam that hands the failure
    // back before `evaluate` reports it and drops it. This port's warning sink
    // is the process-global log file `observability::log` writes, not a handler
    // a test can attach, so what is measured here is the report the production
    // path emits rather than a line read back off disk.
    let (probe, _recorder) = failure_client(case.id);
    let reported = runtime
        .block_on(probe.attempt(&attributes))
        .and_then(Result::err)
        .map(|failure| failure.report());
    answers.insert(
        "logs".to_owned(),
        Value::Object(
            [
                (
                    "count".to_owned(),
                    Value::from(usize::from(reported.is_some())),
                ),
                (
                    "levels".to_owned(),
                    Value::Array(
                        reported
                            .iter()
                            .map(|(level, _)| Value::String(level.as_str().to_owned()))
                            .collect(),
                    ),
                ),
            ]
            .into_iter()
            .collect(),
        ),
    );
    answers
}

// -- featureResolution ----------------------------------------------------

/// One feature definition the capture authored, read back in the order it was
/// written in.
fn authored_feature(case: &Case<'_>, field: &str) -> FeatureDefinition {
    let text = case
        .input_text(field)
        .unwrap_or_else(|| panic!("the {} case carries a {field}", case.id));
    serde_json::from_str(&text)
        .unwrap_or_else(|error| panic!("the {} case {field} parses: {error}", case.id))
}

fn feature_resolution_answer(case: &Case<'_>) -> Map<String, Value> {
    let definition = authored_feature(case, "definition");
    answered("resolved", as_value(definition.resolved_value()))
}

// -- variantResolution, configVariants and variantLabels ------------------

/// A manager hydrated from one authored eval response, or an unread one where
/// the case carries no response at all.
fn manager_over(response: Option<&str>) -> ExperimentManager {
    let mut manager = ExperimentManager::new(RemoteEvalClient::with_url(None));
    if let Some(text) = response {
        manager.hydrate(
            serde_json::from_str(text)
                .unwrap_or_else(|error| panic!("the authored response parses: {error}")),
        );
    }
    manager
}

/// This build's exposures in the shape the corpus records them.
///
/// The reference answers one `ExperimentAssignment` record per experiment
/// (`vibe/core/experiments/resolve.py:59-86` at the pin), where this build
/// answers a map from the feature key to its label. The map carries the
/// record's `experiment_id` and `variation_name` and nothing else, so those are
/// the only two keys written: the rest of the record is not something this
/// build answers, and filling it with nulls would claim answers it never gave.
fn assignment_records(assignments: &BTreeMap<String, String>) -> Value {
    Value::Array(
        assignments
            .iter()
            .map(|(feature, label)| {
                Value::Object(
                    [
                        ("experiment_id".to_owned(), Value::String(feature.clone())),
                        ("variation_name".to_owned(), Value::String(label.clone())),
                    ]
                    .into_iter()
                    .collect(),
                )
            })
            .collect(),
    )
}

fn variant_resolution_answer(case: &Case<'_>) -> Map<String, Value> {
    let manager = manager_over(case.input_text("response").as_deref());
    let mut answers = Map::new();
    answers.insert(
        "knownFeatures".to_owned(),
        manager.export_state().map_or(Value::Null, |state| {
            let mut keys = state.features.keys().map(str::to_owned).collect::<Vec<_>>();
            keys.sort();
            Value::Array(keys.into_iter().map(Value::String).collect())
        }),
    );
    answers.insert(
        "variants".to_owned(),
        by_experiment(|name| Value::String(manager.variant(name))),
    );
    answers.insert(
        "variantsOrNone".to_owned(),
        by_experiment(|name| {
            manager
                .variant_or_none(name)
                .map_or(Value::Null, Value::String)
        }),
    );
    answers
}

fn config_variants_answer(case: &Case<'_>) -> Map<String, Value> {
    let manager = manager_over(case.input_text("response").as_deref());
    let mut answers = Map::new();
    answers.insert(
        "assignments".to_owned(),
        assignment_records(&manager.assignments()),
    );
    answers.insert(
        "configVariants".to_owned(),
        serde_json::to_value(manager.config_variants()).unwrap_or(Value::Null),
    );
    answers
}

fn variant_labels_answer(case: &Case<'_>) -> Map<String, Value> {
    let definition = case
        .input_text("definition")
        .unwrap_or_else(|| panic!("the {} case carries a definition", case.id));
    let response = format!(
        r#"{{"features": {{"{}": {definition}}}}}"#,
        ExperimentName::SystemPrompt.key()
    );
    let manager = manager_over(Some(&response));
    let assignments = manager.assignments();
    let mut answers = Map::new();
    answers.insert(
        "reported".to_owned(),
        Value::Bool(assignments.contains_key(ExperimentName::SystemPrompt.key())),
    );
    answers.insert("assignments".to_owned(), assignment_records(&assignments));
    answers
}

// -- configMapping and layerPrecedence ------------------------------------

/// The prompt resolution the mapping is measured over.
///
/// The reference mapper calls `load_system_prompt`, which answers from a
/// bundled prompt file or from one of the custom prompt directories, and drops
/// the field when it raises. What the *mapping* does with that answer is what
/// this family measures, so the replay hands this port's own
/// [`PromptResolver`] over a directory seeded with the reference's five bundled
/// identifiers. Which identifiers a shipped installation carries is a separate
/// question this family does not decide: US-012 hands the session's own
/// resolver to the same parameter.
struct OraclePrompts {
    root: tempfile::TempDir,
    resolver: PromptResolver,
}

impl OraclePrompts {
    fn seeded() -> Self {
        let root = tempfile::tempdir().expect("a prompt directory");
        let mut builtins = BTreeMap::new();
        for identifier in ["cli", "explore", "tests", "lean", "minimal"] {
            let path = root.path().join(format!("{identifier}.md"));
            fs::write(&path, "seeded system prompt").expect("a seeded prompt file");
            builtins.insert(identifier.to_owned(), path);
        }
        let resolver = PromptResolver::new(Vec::new(), Vec::new(), builtins, false);
        Self { root, resolver }
    }

    fn resolves(&self) -> impl Fn(&str) -> bool + '_ {
        // The directory has to outlive the predicate, which is what borrowing
        // it here spells out.
        let _ = &self.root;
        |prompt_id: &str| self.resolver.resolve(prompt_id).is_ok()
    }
}

/// The configuration variants one case is driven over.
fn case_variants(case: &Case<'_>) -> BTreeMap<String, String> {
    case.object
        .get("variants")
        .and_then(Value::as_object)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|(key, value)| {
                    value.as_str().map(|text| (key.clone(), text.to_owned()))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn config_mapping_answer(case: &Case<'_>) -> Map<String, Value> {
    let mut answers = Map::new();
    if case.id == "persisting-is-refused" {
        // The reference refuses a write to the layer by raising from
        // `GrowthbookLayer._save_to_store`. Here the refusal is structural:
        // every write is addressed to a [`ConfigTarget`], and the match below
        // is exhaustive over the three this port declares, none of which names
        // the experiments layer, so the attempt has no method to reach. The
        // corpus records the reference's exception name and this stands for it,
        // the same way `setup_auth_parity_tests` maps a typed refusal onto the
        // exception the reference raises.
        let refusal = ConfigTarget::User;
        let addressable = match refusal {
            ConfigTarget::User | ConfigTarget::Project | ConfigTarget::Ephemeral => false,
        };
        answers.insert(
            "data".to_owned(),
            Value::Object(
                [(
                    "refusal".to_owned(),
                    if addressable {
                        Value::Null
                    } else {
                        Value::String("NotImplementedError".to_owned())
                    },
                )]
                .into_iter()
                .collect(),
            ),
        );
        answers.insert("hasFingerprint".to_owned(), Value::Null);
        return answers;
    }
    let prompts = OraclePrompts::seeded();
    let layer = ExperimentsLayer::from_variants(&case_variants(case), &prompts.resolves());
    answers.insert(
        "data".to_owned(),
        serde_json::to_value(layer.values()).unwrap_or(Value::Null),
    );
    answers.insert(
        "hasFingerprint".to_owned(),
        Value::Bool(layer.fingerprint().is_some()),
    );
    answers
}

/// The providers and models every precedence case is loaded over, spelled the
/// way `PRECEDENCE_CASES` in `scripts/parity/experiments.py` spells them. A
/// case's own lines are prepended, because a bare key written after a table
/// would land inside it.
const ORACLE_USER_DOCUMENT: &str = r#"
[[providers]]
name = "mistral"
api_base = "https://api.mistral.ai/v1"
api_key_env_var = "ORACLE_MISTRAL_KEY"
backend = "mistral"

[[models]]
name = "oracle-model"
provider = "mistral"
alias = "oracle"
"#;

/// One case input as the TOML table it composes, or an empty one.
fn case_table(case: &Case<'_>, field: &str) -> Table {
    case.object
        .get(field)
        .and_then(Value::as_object)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|(key, value)| {
                    Table::try_from(Value::Object(
                        [(key.clone(), value.clone())].into_iter().collect(),
                    ))
                    .ok()
                })
                .fold(Table::new(), |mut merged, entry| {
                    merged.extend(entry);
                    merged
                })
        })
        .unwrap_or_default()
}

fn layer_precedence_answer(case: &Case<'_>) -> Map<String, Value> {
    let temporary = tempfile::tempdir().expect("a precedence scratch directory");
    let home = temporary.path().join("home/.vibe");
    let working = temporary.path().join("project");
    fs::create_dir_all(&home).expect("the home directory");
    fs::create_dir_all(working.join(".vibe")).expect("the project directory");
    fs::write(
        home.join("config.toml"),
        format!(
            "{}{ORACLE_USER_DOCUMENT}",
            case.input_str("user").unwrap_or_default()
        ),
    )
    .expect("the user document writes");
    if let Some(project) = case.input_str("project") {
        fs::write(working.join(".vibe/config.toml"), project).expect("the project document writes");
    }
    let environment = case
        .object
        .get("environment")
        .and_then(Value::as_object)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|(key, value)| {
                    value.as_str().map(|text| (key.clone(), text.to_owned()))
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let prompts = OraclePrompts::seeded();
    let snapshot = LayeredConfig::new(
        ConfigPaths {
            vibe_home: home,
            working_directory: working,
        },
        default_document(),
    )
    .with_project_trusted(true)
    .with_environment(environment)
    .with_runtime_overrides(case_table(case, "overrides"))
    .with_experiment_variants(&case_variants(case), &prompts.resolves())
    .load()
    .unwrap_or_else(|error| panic!("the {} case composes: {error}", case.id));

    // The fields a case reads are the ones its expectation names, so a case
    // covering one field does not answer for five.
    let expected = case
        .object
        .get("effective")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let effective = expected
        .keys()
        .map(|field| {
            let value = match field.as_str() {
                "resolvedDefaultAlias" => default_model_alias(&snapshot.effective)
                    .map_or(Value::Null, |alias| Value::String(alias.to_owned())),
                "activeModelAlias" => snapshot
                    .active_model_alias()
                    .map_or(Value::Null, |alias| Value::String(alias.to_owned())),
                name => snapshot
                    .effective
                    .get(name)
                    .and_then(|value| serde_json::to_value(value).ok())
                    .unwrap_or(Value::Null),
            };
            (field.clone(), value)
        })
        .collect::<Map<_, _>>();
    answered("effective", Value::Object(effective))
}

// -- sessionGates ---------------------------------------------------------

/// The documents every session scenario is composed over, spelled the way
/// `GATE_CONFIGURATIONS` in `scripts/parity/experiments.py` spells them. The
/// script authored them, so reproducing them here copies nothing the licensing
/// boundary protects.
const GATE_CONFIGURATIONS: [(&str, &str); 7] = [
    (
        "mistral-active",
        r#"
enable_telemetry = true
active_model = "oracle"

[experiments]
enable = true

[[providers]]
name = "mistral"
api_base = "https://api.mistral.ai/v1"
api_key_env_var = "ORACLE_MISTRAL_KEY"
backend = "mistral"

[[models]]
name = "oracle-model"
provider = "mistral"
alias = "oracle"
"#,
    ),
    (
        "telemetry-disabled",
        r#"
enable_telemetry = false
active_model = "oracle"

[experiments]
enable = true

[[providers]]
name = "mistral"
api_base = "https://api.mistral.ai/v1"
api_key_env_var = "ORACLE_MISTRAL_KEY"
backend = "mistral"

[[models]]
name = "oracle-model"
provider = "mistral"
alias = "oracle"
"#,
    ),
    (
        "experiments-disabled",
        r#"
enable_telemetry = true
active_model = "oracle"

[experiments]
enable = false

[[providers]]
name = "mistral"
api_base = "https://api.mistral.ai/v1"
api_key_env_var = "ORACLE_MISTRAL_KEY"
backend = "mistral"

[[models]]
name = "oracle-model"
provider = "mistral"
alias = "oracle"
"#,
    ),
    (
        "both-gates-off",
        r#"
enable_telemetry = false
active_model = "oracle"

[experiments]
enable = false

[[providers]]
name = "mistral"
api_base = "https://api.mistral.ai/v1"
api_key_env_var = "ORACLE_MISTRAL_KEY"
backend = "mistral"

[[models]]
name = "oracle-model"
provider = "mistral"
alias = "oracle"
"#,
    ),
    (
        "third-party-only",
        r#"
enable_telemetry = true
active_model = "oracle"

[experiments]
enable = true

[[providers]]
name = "third-party"
api_base = "https://third-party.example.test/v1"
api_key_env_var = "ORACLE_THIRD_PARTY_KEY"

[[models]]
name = "oracle-model"
provider = "third-party"
alias = "oracle"
"#,
    ),
    (
        "mistral-without-a-resolvable-key",
        r#"
enable_telemetry = true
active_model = "oracle"

[experiments]
enable = true

[[providers]]
name = "mistral"
api_base = "https://api.mistral.ai/v1"
api_key_env_var = "ORACLE_ABSENT_KEY"
backend = "mistral"

[[models]]
name = "oracle-model"
provider = "mistral"
alias = "oracle"
"#,
    ),
    (
        "custom-system-prompt",
        r#"
enable_telemetry = true
active_model = "oracle"
system_prompt_id = "lean"

[experiments]
enable = true

[[providers]]
name = "mistral"
api_base = "https://api.mistral.ai/v1"
api_key_env_var = "ORACLE_MISTRAL_KEY"
backend = "mistral"

[[models]]
name = "oracle-model"
provider = "mistral"
alias = "oracle"
"#,
    ),
];

/// The identity a scenario that resolves one answers, and the organization it
/// carries into the attributes.
const ORACLE_ORGANIZATION: &str = "oracle-organization";

/// The eval body every gate scenario is answered with unless the case names a
/// failure: one forced system-prompt feature carrying a confirmed track.
const GATE_RESPONSE: &str = r#"{"features": {"vibe_cli_system_prompt": {"defaultValue": "cli",
    "rules": [{"force": "tests", "tracks": [{"experiment": {"key": "vibe_cli_system_prompt"},
    "result": {"key": "1", "variationId": 1, "inExperiment": true}}]}]}}}"#;

/// The document one gate configuration names.
fn gate_document(configuration: &str) -> &'static str {
    GATE_CONFIGURATIONS
        .into_iter()
        .find(|(id, _)| *id == configuration)
        .map(|(_, document)| document)
        .unwrap_or_else(|| panic!("{configuration} is a gate configuration this replay knows"))
}

/// The configuration one gate document validates to: the declared defaults with
/// the document written over them, key by key.
///
/// This is the composition the capture drove the reference over, which reads
/// one authored document against its schema defaults. It is deliberately not a
/// [`LayeredConfig`] load: the shipped defaults layer declares its own
/// `providers`, and a *union* merge would put a Mistral provider under the
/// `third-party-only` case that the document does not declare. What this family
/// measures is what the session helpers read out of a configuration, and the
/// composition of the configuration itself is `layerPrecedence`'s subject.
fn gate_effective(configuration: &str) -> Table {
    let authored = gate_document(configuration)
        .parse::<Table>()
        .unwrap_or_else(|error| panic!("the {configuration} document parses: {error}"));
    let mut composed = default_document();
    write_over(&mut composed, &authored);
    composed
}

/// `overlay` written over `target`: a table is merged key by key and everything
/// else, arrays included, replaces what it covers.
fn write_over(target: &mut Table, overlay: &Table) {
    for (key, value) in overlay {
        match (target.get_mut(key), value) {
            (Some(toml::Value::Table(existing)), toml::Value::Table(incoming)) => {
                write_over(existing, incoming);
            }
            _ => {
                target.insert(key.clone(), value.clone());
            }
        }
    }
}

/// How the capture's exported variables become credentials, without any of them
/// being read off this machine's environment.
fn gate_credentials(name: &str) -> Option<String> {
    SENTINELS
        .into_iter()
        .find(|(variable, _)| *variable == name)
        .map(|(_, sentinel)| sentinel.to_owned())
}

fn session_gates_answer(case: &Case<'_>, runtime: &Runtime) -> Map<String, Value> {
    let configuration = case
        .input_str("configuration")
        .expect("every session case names its configuration");
    let effective = gate_effective(configuration);
    let mut answers = Map::new();
    let scenario = case.id.rsplit('/').next().unwrap_or_default();
    match case.input_str("helper") {
        Some("hydrate") => {
            let transport = Arc::new(RecordingTransport::answering(GATE_RESPONSE));
            let mut manager = ExperimentManager::new(RemoteEvalClient::with_transport(
                build_eval_url(ORACLE_API_HOST, ORACLE_CLIENT_KEY),
                Arc::clone(&transport) as _,
            ));
            // The reference reads the field off the session metadata, where an
            // absent metadata and a metadata carrying no experiments are the
            // same absent state, so both answer the same here.
            let state = (scenario == "metadata-with-experiments").then(|| {
                serde_json::from_str::<EvalResponse>(GATE_RESPONSE)
                    .expect("the authored gate response validates")
            });
            let hydrated = hydrate_experiments_from_session(&effective, &mut manager, state);
            answers.insert("returned".to_owned(), Value::Bool(hydrated));
            answers.insert(
                "evalRequests".to_owned(),
                Value::from(transport.request_count()),
            );
            answers.insert("identityRequests".to_owned(), Value::from(0));
            answers.insert("identityTimeout".to_owned(), Value::Null);
            answers.insert("persisted".to_owned(), Value::from(0));
            answers.insert("organizationId".to_owned(), Value::Null);
        }
        _ => {
            let outcome = match scenario {
                "eval-fails" => Outcome::Answer {
                    status: 500,
                    body: r#"{"features": {}}"#.to_owned(),
                },
                "eval-returns-nothing" => Outcome::ok(r#"{"features": {}}"#),
                _ => Outcome::ok(GATE_RESPONSE),
            };
            let identity = (scenario == "identity").then_some(ORACLE_ORGANIZATION);
            let transport = Arc::new(RecordingTransport::new(outcome));
            let mut manager = ExperimentManager::new(RemoteEvalClient::with_transport(
                build_eval_url(ORACLE_API_HOST, ORACLE_CLIENT_KEY),
                Arc::clone(&transport) as _,
            ));
            let resolver = RecordingResolver::answering(identity, SENTINELS[1].1);
            let sink = RecordingSink::default();
            let refreshed = runtime.block_on(initialize_experiments(
                &effective,
                &gate_credentials,
                &mut manager,
                None,
                &resolver,
                &sink,
            ));
            let calls = resolver.calls();
            // The reference returns `(refreshed, user_plan)`
            // (`vibe/core/experiments/session.py:103-152` at the pin). This
            // build's helper resolves no plan, so its answer is the refresh
            // flag beside an absent plan, which is what the reference answers
            // too wherever its plan lookup comes back empty.
            answers.insert(
                "returned".to_owned(),
                Value::Array(vec![Value::Bool(refreshed), Value::Null]),
            );
            answers.insert(
                "evalRequests".to_owned(),
                Value::from(transport.request_count()),
            );
            answers.insert("identityRequests".to_owned(), Value::from(calls.len()));
            answers.insert(
                "identityTimeout".to_owned(),
                calls
                    .first()
                    .and_then(|call| call.timeout)
                    .map_or(Value::Null, seconds),
            );
            answers.insert("persisted".to_owned(), Value::from(sink.count()));
            answers.insert(
                "organizationId".to_owned(),
                transport
                    .requests()
                    .first()
                    .and_then(|request| request.payload.pointer("/attributes/organizationId"))
                    .cloned()
                    .unwrap_or(Value::Null),
            );
        }
    }
    answers
}

// -- attributes -----------------------------------------------------------

/// The launch contexts the capture builds attributes from, by the name it gives
/// each one.
fn launch_context(context: &str) -> Option<LaunchContext> {
    let launch = |entrypoint: &str, client: &str, client_version: &str, terminal: Option<&str>| {
        LaunchContext {
            agent_entrypoint: entrypoint.to_owned(),
            agent_version: "9.9.9".to_owned(),
            client_name: client.to_owned(),
            client_version: client_version.to_owned(),
            terminal_emulator: terminal.map(ToOwned::to_owned),
        }
    };
    match context {
        "no-launch-context" => None,
        "cli" => Some(launch("cli", "vibe", "9.9.9", None)),
        "cli-in-vscode" => Some(launch("cli", "vibe", "9.9.9", Some("vscode"))),
        "acp-in-cursor" => Some(launch("acp", "zed", "0.1.0", Some("cursor"))),
        "programmatic" => Some(launch("programmatic", "harness", "2.0.0", Some("unknown"))),
        other => panic!("{other} is a launch context this replay knows"),
    }
}

/// One set of attributes as the object the capture records: every declared key,
/// including the ones an absent option leaves null on the way out.
fn attribute_object(attributes: &ExperimentAttributes) -> Value {
    let text =
        |value: Option<&String>| value.map_or(Value::Null, |value| Value::String(value.clone()));
    Value::Object(
        [
            ("userId", Value::String(attributes.user_id.clone())),
            ("entrypoint", Value::String(attributes.entrypoint.clone())),
            (
                "agent_version",
                Value::String(attributes.agent_version.clone()),
            ),
            ("client_name", text(attributes.client_name.as_ref())),
            ("client_version", text(attributes.client_version.as_ref())),
            ("os", Value::String(attributes.os.clone())),
            (
                "terminal_emulator",
                text(attributes.terminal_emulator.as_ref()),
            ),
            (
                "custom_system_prompt",
                Value::Bool(attributes.custom_system_prompt),
            ),
            ("organizationId", text(attributes.organization_id.as_ref())),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value))
        .collect(),
    )
}

fn attributes_answer(case: &Case<'_>) -> Map<String, Value> {
    let configuration = match case.input_str("document") {
        Some("custom-prompt") => "custom-system-prompt",
        _ => "mistral-active",
    };
    let context = case
        .input_str("context")
        .expect("every attributes case names its launch context");
    let launch = launch_context(context);
    let attributes = build_attributes(
        &gate_effective(configuration),
        SENTINELS[1].1,
        launch.as_ref(),
        (context != "no-launch-context").then(|| ORACLE_ORGANIZATION.to_owned()),
    );
    let posted = serde_json::to_value(&attributes).expect("the attributes serialize");
    let mut answers = Map::new();
    answers.insert(
        "attributes".to_owned(),
        scrub(&attribute_object(&attributes)),
    );
    answers.insert("payloadKeys".to_owned(), sorted_keys(&posted));
    answers.insert(
        "credentialVariable".to_owned(),
        sentinel_variable(&attributes.user_id)
            .map_or(Value::Null, |variable| Value::String(variable.to_owned())),
    );
    answers
}

// --------------------------------------------------------------------------
// The replay
// --------------------------------------------------------------------------

/// One expectation with every null a TOML document cannot hold dropped.
///
/// The reference dumps a field it validated to `None` as an explicit null, and
/// TOML carries none, so the key is simply absent here. Dropping it from the
/// expectation is the same normalization `config/surface_parity_tests.rs`
/// applies to the merged document; a key this port fills where the reference
/// answered null still diverges, because the answer keeps it.
fn without_reference_nulls(value: &Value) -> Value {
    match value {
        Value::Object(entries) => Value::Object(
            entries
                .iter()
                .filter(|(_, value)| !value.is_null())
                .map(|(key, value)| (key.clone(), without_reference_nulls(value)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(without_reference_nulls).collect()),
        other => other.clone(),
    }
}

/// Replays one family, comparing every answer field of every case.
fn run_family(
    corpus: &Map<String, Value>,
    ordered: &JsonValue,
    family: &Family,
    runtime: &Runtime,
    report: &mut Report,
) {
    let declared = family
        .inputs
        .iter()
        .chain(family.answers.iter())
        .copied()
        .collect::<BTreeSet<_>>();
    let ordered_cases = ordered
        .as_object()
        .and_then(|corpus| corpus.get(family.name))
        .and_then(|cases| match cases {
            JsonValue::Array(items) => Some(items),
            _ => None,
        })
        .unwrap_or_else(|| panic!("the ordered corpus carries the {} family", family.name));
    for (position, case) in cases(corpus, family.name).iter().enumerate() {
        let object = case
            .as_object()
            .unwrap_or_else(|| panic!("{} cases are objects", family.name));
        let carried = object
            .keys()
            .map(String::as_str)
            .filter(|key| *key != "id")
            .collect::<BTreeSet<_>>();
        assert!(
            carried.iter().all(|key| declared.contains(key)),
            "the {} case {} carries fields this replay does not read: {:?}; declare them as an \
             input or as an answer rather than leaving them unread",
            family.name,
            case_id(case),
            carried.difference(&declared).collect::<Vec<_>>()
        );
        let ordered_case = ordered_cases
            .get(position)
            .unwrap_or_else(|| panic!("the two readings of {} agree in length", family.name));
        let identifier = case_id(case);
        assert_eq!(
            ordered_case
                .as_object()
                .and_then(|entry| entry.get("id"))
                .and_then(JsonValue::as_str),
            Some(identifier),
            "the two readings of {} agree case by case",
            family.name
        );
        let read = Case {
            id: identifier,
            object,
            ordered: ordered_case,
        };
        let answers = port_case(family.name, &read, runtime);
        for field in family.answers {
            let Some(expected) = object.get(*field) else {
                continue;
            };
            let actual = answers.as_ref().and_then(|answers| answers.get(*field));
            let expected = if family.toml_document {
                &without_reference_nulls(expected)
            } else {
                expected
            };
            report.check(family.name, field, identifier, expected, actual);
        }
    }
}

#[test]
fn every_corpus_key_is_a_family_this_replay_reads() {
    let corpus = corpus();
    let declared = FAMILIES
        .iter()
        .map(|family| family.name)
        .chain(METADATA)
        .collect::<BTreeSet<_>>();
    let carried = corpus.keys().map(String::as_str).collect::<BTreeSet<_>>();
    assert_eq!(
        carried, declared,
        "the corpus and this replay disagree on which families exist; regenerate with \
         {CAPTURE_SCRIPT} or declare the family here"
    );
}

#[test]
fn every_ledger_entry_names_a_declared_family() {
    let declared = FAMILIES
        .iter()
        .map(|family| family.name)
        .collect::<BTreeSet<_>>();
    let orphans = DIVERGENCES
        .iter()
        .map(|(entry, _)| *entry)
        .filter(|entry| {
            entry
                .split('/')
                .next()
                .is_none_or(|family| !declared.contains(family))
        })
        .collect::<Vec<_>>();
    assert!(
        orphans.is_empty(),
        "these ledger entries name a family the corpus does not carry: {orphans:?}"
    );
}

/// The two failure modes the replay exists for, proven on a report the test
/// builds rather than on the corpus: a divergence the ledger does not name has
/// to be reported with its family, its case and both values, and a ledger entry
/// whose divergence stopped reproducing has to be reported as stale.
#[test]
fn the_ledger_reports_an_unrecorded_divergence_and_a_stale_entry() {
    let ledger = [("evalUrl/url/named", "recorded")];
    let mut report = Report::default();
    report.check(
        "evalUrl",
        "url",
        "unnamed",
        &Value::String("https://reference.example.test".to_owned()),
        Some(&Value::String("https://port.example.test".to_owned())),
    );
    let (unrecorded, stale) = audit(&report, "evalUrl", &ledger);
    assert_eq!(unrecorded.len(), 1, "the unnamed divergence is reported");
    let reported = &unrecorded[0];
    assert!(reported.starts_with("evalUrl/url/unnamed:"), "{reported}");
    assert!(
        reported.contains("https://reference.example.test"),
        "{reported}"
    );
    assert!(reported.contains("https://port.example.test"), "{reported}");
    assert_eq!(
        stale,
        vec!["evalUrl/url/named".to_owned()],
        "an entry whose case stopped diverging is stale"
    );

    // An absent surface reads as a divergence too, which is what keeps a
    // ledgered gap from passing quietly once it closes.
    let mut absent = Report::default();
    absent.check("evalUrl", "url", "named", &Value::Null, None);
    let (unrecorded, stale) = audit(&absent, "evalUrl", &ledger);
    assert!(unrecorded.is_empty(), "the ledger names it: {unrecorded:?}");
    assert!(stale.is_empty(), "it still diverges: {stale:?}");
    assert!(absent.divergences[0].ends_with("port absent"));
}

/// The one tolerance the comparison carries, proven in both directions: a
/// reference null compares equal to an absent key, and a key this port fills
/// where the reference answered null still diverges.
///
/// Only `layerPrecedence` reads answers out of a TOML document, and only
/// `routed_model_config` exercises the tolerance there: `cached_input_price`
/// defaults to null upstream and TOML carries no null, so the key is absent
/// rather than empty. Every other field of that definition is compared for
/// equality.
#[test]
fn a_reference_null_compares_equal_to_the_absent_key_toml_leaves() {
    let reference = serde_json::json!({"a": 1, "cached_input_price": null});
    assert_eq!(
        without_reference_nulls(&reference),
        serde_json::json!({"a": 1})
    );

    let mut report = Report::default();
    report.check(
        "layerPrecedence",
        "effective",
        "filled",
        &without_reference_nulls(&reference),
        Some(&serde_json::json!({"a": 1, "cached_input_price": 0.5})),
    );
    assert_eq!(
        report.conformant, 0,
        "a value where the reference answered null is still a divergence"
    );
}

#[test]
fn the_committed_corpus_replays_against_this_port() {
    let raw = corpus_text();
    let ordered = ordered_corpus(&raw);
    let corpus = corpus();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("a current-thread runtime builds");
    println!(
        "experiments: divergence ledger ({} entries)",
        DIVERGENCES.len()
    );
    for (case, reason) in DIVERGENCES {
        println!("  {case}: {reason}");
    }
    let mut comparisons = 0;
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for family in FAMILIES {
        let mut report = Report::default();
        run_family(&corpus, &ordered, family, &runtime, &mut report);
        let total = settle(&report, family.name);
        counts.insert(family.name, total);
        comparisons += total;
    }
    println!(
        "experiments: {comparisons} comparisons across {} families replayed at {}",
        FAMILIES.len(),
        &REFERENCE_COMMIT[..12],
    );
    assert!(
        comparisons >= MINIMUM_COMPARISONS,
        "the corpus replays {comparisons} comparisons, below the {MINIMUM_COMPARISONS} floor; \
         regenerate it with {CAPTURE_SCRIPT}"
    );
}

/// The corpus is only an oracle for as long as it still describes the pinned
/// reference. This probe recaptures it where the checkout is present and on the
/// pin, and skips everywhere else naming the pin and the way back.
#[test]
fn the_committed_corpus_still_matches_the_pinned_reference() {
    let root = reference_root();
    if let Some(reason) = off_pin_reason(&root, "experiments") {
        eprintln!("{reason}");
        eprintln!("the committed corpus replayed regardless; restore with `{RESTORE_COMMAND}`");
        return;
    }
    let repository = repo_root();
    let script = repository.join(CAPTURE_SCRIPT);
    let recaptured = repository.join("target/experiments-corpus.json");
    let promo = repository.join("target/experiments-promo-corpus.json");
    let output = Command::new("python3")
        .arg(&script)
        .args(["--reference".as_ref(), root.as_os_str()])
        .arg("--output")
        .arg(repository.join("target/experiments-full.json"))
        .arg("--corpus")
        .arg(&recaptured)
        .arg("--promo-corpus")
        .arg(&promo)
        .current_dir(&repository)
        .output()
        .expect("the experiments capture script runs");
    assert!(
        output.status.success(),
        "the experiments capture failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let fresh = fs::read_to_string(&recaptured).expect("the recaptured corpus is readable");
    let committed =
        fs::read_to_string(repository.join(CORPUS_RELATIVE)).expect("the corpus is readable");
    let fresh: Value = serde_json::from_str(&fresh).expect("the recaptured corpus parses");
    let committed: Value = serde_json::from_str(&committed).expect("the corpus parses");
    assert_eq!(
        fresh, committed,
        "the pinned reference no longer answers what the committed corpus records; regenerate it \
         with `{CAPTURE_SCRIPT}`"
    );
}
