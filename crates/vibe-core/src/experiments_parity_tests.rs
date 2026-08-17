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

use crate::config::registry::default_document;
use crate::experiments::recorder::{Outcome, RecordingTransport};
use crate::experiments::{
    BUCKETING_KEY_LENGTH, EVAL_PATH_TEMPLATE, EVAL_REQUEST_TIMEOUT, EvalPayload,
    ExperimentAttributes, ExperimentManager, ExperimentName, FeatureDefinition, JsonValue,
    RemoteEvalClient, build_eval_url, hash_api_key,
};
use crate::parity::{REFERENCE_COMMIT, RESTORE_COMMAND, off_pin_reason, reference_root};
use crate::telemetry::platform_id;

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
    },
    Family {
        name: "bucketingKey",
        inputs: &["variable"],
        answers: &["bucketingKey", "length", "stable", "hexadecimal"],
    },
    Family {
        name: "evalUrl",
        inputs: &["apiHost", "clientKey"],
        answers: &["url"],
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
    },
    Family {
        name: "featureResolution",
        inputs: &["definition"],
        answers: &["resolved"],
    },
    Family {
        name: "variantResolution",
        inputs: &["response"],
        answers: &["knownFeatures", "variants", "variantsOrNone"],
    },
    Family {
        name: "configVariants",
        inputs: &["response"],
        answers: &["assignments", "configVariants"],
    },
    Family {
        name: "variantLabels",
        inputs: &["definition"],
        answers: &["assignments", "reported"],
    },
    Family {
        name: "configMapping",
        inputs: &["variants"],
        answers: &["data", "hasFingerprint"],
    },
    Family {
        name: "layerPrecedence",
        inputs: &["user", "project", "environment", "overrides", "variants"],
        answers: &["effective"],
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
    },
    Family {
        name: "attributes",
        inputs: &["document", "context"],
        answers: &["attributes", "payloadKeys", "credentialVariable"],
    },
];

/// Cases where this build answers something other than the reference, each with
/// the reason and the story that closes it.
///
/// Every entry here is open work rather than an accepted divergence: the layer
/// and the session lifecycle are still absent and answer [`None`]. The
/// staleness check is what forces a row out once its behavior conforms, so the
/// ledger cannot outlive the gap it records, and EP-003 closing the engine took
/// its fifteen rows out in the same change that filled [`port_case`].
const DIVERGENCES: &[(&str, &str)] = &[
    // -- EP-005: the session lifecycle --------------------------------------
    (
        "constants/value/identityPath",
        "OPEN: US-011 ports the identity gateway the organization attribute depends on",
    ),
    ("constants/value/identityTimeoutSeconds", "OPEN: as above"),
    (
        "sessionGates/*",
        "OPEN: US-012 ports the two session helpers, their gates and their persistence",
    ),
    (
        "attributes/*",
        "OPEN: US-012 builds the nine attributes from a launch context",
    ),
    // -- EP-004: the GrowthBook configuration layer -------------------------
    (
        "constants/value/layerName",
        "OPEN: US-009 names the layer that turns a variant into a configuration value",
    ),
    (
        "constants/value/configuredFields",
        "OPEN: US-009 maps the three experiments onto the four configuration fields",
    ),
    (
        "configMapping/*",
        "OPEN: US-009 ports the three mappers, the empty-snapshot branches and the read-only \
         refusal",
    ),
    (
        "layerPrecedence/*",
        "OPEN: US-009 fills the layer and US-010 reseats it below the selected TOML, where the \
         reference puts it; the layer socket exists here but nothing maps a variant into it",
    ),
];

/// One family's shape: which case fields the capture authored and which ones
/// both sides answer.
struct Family {
    name: &'static str,
    inputs: &'static [&'static str],
    answers: &'static [&'static str],
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

/// The bucketing digest every request scenario posts, authored rather than
/// derived so no scenario needs a credential to build a request.
const ORACLE_USER_ID: &str = "0123456789abcdef0123456789abcdef";

/// What the capture writes in place of the machine's own platform.
const PLATFORM_ID_PLACEHOLDER: &str = "{platformId}";

/// The credential sentinels the capture exports, mirroring `SENTINELS` in
/// `scripts/parity/experiments.py`. They are authored strings that stand where
/// an API key would, which is what lets the bucketing family be measured
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
        "evalTimeoutSeconds" => timeout_seconds(),
        "bucketingKeyLength" => Value::from(BUCKETING_KEY_LENGTH),
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
    serde_json::Number::from_f64(EVAL_REQUEST_TIMEOUT.as_secs_f64())
        .map_or(Value::Null, Value::Number)
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

/// The machine's own platform replaced by the placeholder the capture records,
/// so the corpus stays portable.
fn scrub(value: &Value) -> Value {
    match value {
        Value::String(text) if *text == platform_id() => {
            Value::String(PLATFORM_ID_PLACEHOLDER.to_owned())
        }
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
        serde_json::to_value(manager.assignments()).unwrap_or(Value::Null),
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
        serde_json::to_value(manager.assignments()).unwrap_or(Value::Null),
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
    answers.insert(
        "assignments".to_owned(),
        serde_json::to_value(assignments).unwrap_or(Value::Null),
    );
    answers
}

// --------------------------------------------------------------------------
// The replay
// --------------------------------------------------------------------------

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
