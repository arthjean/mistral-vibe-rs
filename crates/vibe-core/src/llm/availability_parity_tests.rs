//! Replays the utility model families of the LLM backend corpus.
//!
//! `scripts/parity/llm_backends.py` recorded four of them from the pinned
//! reference: which model a background nicety runs on once the availability
//! cache holds a given set of verdicts (`utilitySelections`), the key a
//! verdict is cached under (`availabilityKeys`), the fast-model probe a
//! titled session runs as it opens (`availabilityProbes`), round after round
//! against the scripted stand-in on a hand-moved clock: every request the
//! probe sent, the selection it left and the cache file byte for byte, and
//! one utility completion per case (`utilityCompletions`): the request with
//! the label and attribution its metadata carries, the `vibe.request_sent`
//! it reported and the content it answered. This module runs the same cases
//! through [`super::utility`] and [`super::availability`] and compares every
//! field.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use super::availability::{CACHE_FILE, ModelAvailability, cache_key};
use super::backend_parity_tests::{
    CORPUS, FakeClock, FakeVertex, StandIn, closed_port, differences, resolve, substitute,
    toml_table,
};
use super::retry::{Clock, SleepFuture, SleepKind};
use super::utility::{self, UtilityFeature};
use super::{BackendContext, Credentials, MapCredentials};
use crate::experiments::{JsonValue, OrderedMap};
use crate::provider::config::{
    ApiSettings, BackendKind, ModelConfig, ModelRouting, ProviderConfig, UtilityModels,
};
use crate::telemetry::{ClientTelemetry, LaunchContext, TelemetryCallType, TelemetryRecord};

/// The key every case's key variable holds. The oracle's `KEY`.
const KEY: &str = "oracle-key";
/// The wall clock the oracle froze the cache at. The oracle's `PROBE_NOW`.
const PROBE_NOW: u64 = 1_800_000_000;

/// The wall and monotonic clocks the cache reads, moved by hand as the
/// oracle's `ProbeClock` is.
struct HandClock(Mutex<(u64, f64)>);

impl HandClock {
    fn new() -> Self {
        Self(Mutex::new((PROBE_NOW, 5_000.0)))
    }

    fn advance(&self, seconds: u64) {
        let mut state = self.0.lock().expect("clock lock");
        state.0 += seconds;
        #[allow(clippy::cast_precision_loss)]
        {
            state.1 += seconds as f64;
        }
    }

    fn wall(&self) -> u64 {
        self.0.lock().expect("clock lock").0
    }
}

impl Clock for HandClock {
    fn monotonic(&self) -> f64 {
        self.0.lock().expect("clock lock").1
    }

    fn now(&self) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(self.wall())
    }

    fn sleep(&self, _seconds: f64, _kind: SleepKind) -> SleepFuture {
        Box::pin(std::future::ready(()))
    }

    fn jitter(&self) -> f64 {
        0.5
    }
}

fn provider(entry: &Value) -> ProviderConfig {
    ProviderConfig::from_table(&toml_table(entry)).expect("a case provider reads")
}

/// The routing a case describes: its providers, its active model (if any)
/// and its allowlist.
fn routing(case: &Value, providers: Vec<ProviderConfig>) -> ModelRouting {
    let model = |entry: &Value| {
        ModelConfig::from_table(&toml_table(entry), None).expect("a model entry reads")
    };
    let active = (!case["active"].is_null()).then(|| model(&case["active"]));
    let utility = &case["utilityModels"];
    ModelRouting {
        providers,
        active_alias: active.as_ref().map(|model| model.alias.clone()),
        models: active
            .into_iter()
            .chain(case["models"].as_array().into_iter().flatten().map(model))
            .collect(),
        allowed_models: case["allowedModels"]
            .as_array()
            .expect("an allowlist")
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        api: ApiSettings::default(),
        utility_models: UtilityModels {
            title: utility["title"].as_str().unwrap_or_default().to_owned(),
            smart_approve: utility["smart_approve"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
        },
    }
}

fn feature(value: &Value) -> Option<UtilityFeature> {
    match value.as_str()? {
        "title" => Some(UtilityFeature::Title),
        _ => Some(UtilityFeature::SmartApprove),
    }
}

/// What the oracle's `selection_record` writes.
fn selection_record(
    routing: &ModelRouting,
    credentials: &dyn Credentials,
    availability: &ModelAvailability,
    feature: Option<UtilityFeature>,
) -> Value {
    match utility::select(routing, credentials, availability, feature) {
        Ok(selected) => json!({
            "model": selected.model.name,
            "alias": selected.model.alias,
            "provider": selected.provider.name,
            "fast": selected.is_fast(),
            "temperature": selected.model.temperature,
        }),
        Err(_) => json!({"error": true}),
    }
}

fn candidate(name: &str) -> ModelConfig {
    utility::fast_model_candidates()
        .into_iter()
        .find(|candidate| candidate.name == name)
        .expect("a fast candidate")
}

fn compare(name: &str, reference: &Value, port: &Value, unexplained: &mut Vec<String>) {
    let mut found = Vec::new();
    differences(reference, port, "", &mut found);
    for pointer in found {
        unexplained.push(format!(
            "{name} {pointer}\n    reference: {}\n    port:      {}",
            resolve(reference, &pointer),
            resolve(port, &pointer)
        ));
    }
}

#[test]
fn the_utility_model_is_selected_as_the_corpus_records() {
    let corpus: Value = serde_json::from_str(CORPUS).expect("the corpus parses");
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let mut unexplained = Vec::new();
    let cases = corpus["utilitySelections"]
        .as_array()
        .expect("the corpus holds selections");
    for (index, case) in cases.iter().enumerate() {
        let providers: Vec<ProviderConfig> = case["providers"]
            .as_array()
            .expect("providers")
            .iter()
            .map(provider)
            .collect();
        let routing = routing(case, providers);
        let credentials = MapCredentials(
            case["env"]
                .as_array()
                .expect("an environment")
                .iter()
                .filter_map(Value::as_str)
                .map(|name| (name.to_owned(), KEY.to_owned()))
                .collect(),
        );
        let availability = ModelAvailability::new(
            scratch
                .path()
                .join(format!("selection-{index}"))
                .join(CACHE_FILE),
            Arc::new(HandClock::new()),
        );
        if let Some(mistral) = routing.mistral_provider() {
            for (model, available) in case["verdicts"].as_object().expect("verdicts") {
                availability.remember(
                    &mistral,
                    &candidate(model),
                    &credentials,
                    available.as_bool().expect("a verdict"),
                );
            }
        }
        let port = selection_record(
            &routing,
            &credentials,
            &availability,
            feature(&case["feature"]),
        );
        let reference: serde_json::Map<String, Value> =
            ["model", "alias", "provider", "fast", "temperature", "error"]
                .into_iter()
                .filter_map(|key| case.get(key).map(|value| (key.to_owned(), value.clone())))
                .collect();
        let name = format!(
            "utilitySelections/{}",
            case["name"].as_str().expect("a name")
        );
        compare(&name, &Value::Object(reference), &port, &mut unexplained);
    }
    assert!(
        unexplained.is_empty(),
        "utility selections depart from the corpus:\n{}",
        unexplained.join("\n")
    );
    println!(
        "utility selections: {}/{} conformant",
        cases.len(),
        cases.len()
    );
}

#[test]
fn every_verdict_is_cached_under_the_recorded_key() {
    let corpus: Value = serde_json::from_str(CORPUS).expect("the corpus parses");
    for case in corpus["availabilityKeys"]
        .as_array()
        .expect("the corpus holds keys")
    {
        let mut entry = ProviderConfig::new("p", case["apiBase"].as_str().expect("a base"));
        entry.backend = match case["backend"].as_str() {
            Some("mistral") => BackendKind::Mistral,
            _ => BackendKind::Generic,
        };
        let model = candidate(case["model"].as_str().expect("a model"));
        let credential = case["credential"].as_str().unwrap_or_default();
        assert_eq!(
            Some(cache_key(&entry, &model, credential).as_str()),
            case["key"].as_str(),
            "{case}"
        );
    }
}

/// The cache entries a seed describes, written the way the oracle's
/// `write_seed` writes them through the reference's own `_write_entries`.
fn write_seed(path: &Path, seed: &Value, labels: &BTreeMap<String, String>, now: u64) {
    let mut entries = std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<JsonValue>(&text).ok())
        .and_then(|value| match value {
            JsonValue::Object(entries) => Some(entries),
            _ => None,
        })
        .unwrap_or_default();
    for item in seed.as_array().expect("a seed") {
        if let Some(raw) = item["raw"].as_str() {
            let key = if raw == "$FIRST" {
                labels[utility::FAST_MODEL_NAME].clone()
            } else {
                raw.to_owned()
            };
            let value: JsonValue =
                serde_json::from_value(item["value"].clone()).expect("a raw entry");
            entries.insert(key, value);
        } else {
            let mut fields = OrderedMap::new();
            fields.insert(
                "available".to_owned(),
                JsonValue::Bool(item["available"].as_bool().expect("a verdict")),
            );
            let age = item["age"].as_u64().expect("an age");
            fields.insert(
                "stored_at_timestamp".to_owned(),
                JsonValue::Number((now - age).into()),
            );
            entries.insert(
                labels[item["model"].as_str().expect("a model")].clone(),
                JsonValue::Object(fields),
            );
        }
    }
    std::fs::create_dir_all(path.parent().expect("a parent")).expect("the home exists");
    std::fs::write(path, JsonValue::Object(entries).python_json_compact()).expect("a seed writes");
}

/// The platform a request names is the capturing machine's.
fn normalize_platform(mut requests: Vec<Value>) -> Vec<Value> {
    for request in &mut requests {
        if let Some(metadata) = request
            .pointer_mut("/body/metadata")
            .and_then(Value::as_object_mut)
        {
            for field in ["os", "arch", "os_version"] {
                if let Some(value) = metadata.get_mut(field) {
                    *value = Value::String(format!("<{field}>"));
                }
            }
        }
    }
    requests
}

async fn run_probe_case(case: &Value, home: &Path) -> Value {
    let stand_in =
        StandIn::start(case["responses"].as_array().map_or(&[][..], Vec::as_slice)).await;
    let closed = closed_port();
    let substitutions = [
        ("$BASE", stand_in.base.as_str()),
        ("$CLOSED", closed.as_str()),
    ];
    let providers: Vec<ProviderConfig> = case["providers"]
        .as_array()
        .expect("providers")
        .iter()
        .map(|entry| provider(&substitute(entry, &substitutions)))
        .collect();
    let routing = routing(case, providers);
    let credentials: Arc<dyn Credentials> = Arc::new(MapCredentials(
        case["env"]
            .as_object()
            .expect("an environment")
            .iter()
            .map(|(name, value)| (name.clone(), value.as_str().unwrap_or_default().to_owned()))
            .collect(),
    ));
    let clock = Arc::new(HandClock::new());
    let path = home.join(CACHE_FILE);
    let availability = ModelAvailability::new(path.clone(), Arc::clone(&clock) as Arc<dyn Clock>);
    let context = BackendContext {
        api: ApiSettings::default(),
        clock: Arc::new(FakeClock::new()),
        credentials: Arc::clone(&credentials),
        vertex: Arc::new(FakeVertex(stand_in.base.clone())),
    };
    let labels: BTreeMap<String, String> = routing
        .mistral_provider()
        .map(|mistral| {
            let credential = credentials
                .resolve(&mistral.api_key_env_var)
                .map(|(key, _)| key)
                .unwrap_or_default();
            utility::fast_model_candidates()
                .iter()
                .map(|model| (model.name.clone(), cache_key(&mistral, model, &credential)))
                .collect()
        })
        .unwrap_or_default();
    if let Some(raw) = case["rawFile"].as_str() {
        std::fs::create_dir_all(home).expect("the home exists");
        std::fs::write(&path, raw).expect("a raw file writes");
    }
    if !case["seed"].as_array().is_none_or(Vec::is_empty) {
        write_seed(&path, &case["seed"], &labels, clock.wall());
    }
    let mut rounds = Vec::new();
    for round in case["rounds"].as_array().expect("rounds") {
        clock.advance(round["advance"].as_u64().expect("an advance"));
        if !round["seed"].as_array().is_none_or(Vec::is_empty) {
            write_seed(&path, &round["seed"], &labels, clock.wall());
        }
        let features: Vec<UtilityFeature> = round["features"]
            .as_array()
            .expect("features")
            .iter()
            .filter_map(feature)
            .collect();
        let before = stand_in.requests().len();
        utility::probe_unless_disabled(
            round["disabled"].as_bool().expect("a switch"),
            &routing,
            &features,
            Some(Duration::from_secs_f64(
                round["budget"].as_f64().expect("a budget"),
            )),
            &availability,
            &context,
        )
        .await;
        let file = std::fs::read_to_string(&path).ok().map(|mut text| {
            for (name, key) in &labels {
                text = text.replace(key, &format!("<{name}>"));
            }
            text
        });
        rounds.push(json!({
            "requests": normalize_platform(stand_in.requests()[before..].to_vec()),
            "selection": selection_record(
                &routing,
                credentials.as_ref(),
                &availability,
                Some(UtilityFeature::Title),
            ),
            "file": file,
        }));
    }
    substitute(
        &Value::Array(rounds),
        &[
            (closed.as_str(), "$CLOSED"),
            (stand_in.base.as_str(), "$BASE"),
            (env!("CARGO_PKG_VERSION"), "<version>"),
        ],
    )
}

#[test]
fn the_fast_model_probe_answers_every_case_as_the_corpus_records() {
    let corpus: Value = serde_json::from_str(CORPUS).expect("the corpus parses");
    let cases = corpus["availabilityProbes"]
        .as_array()
        .expect("the corpus holds probe cases")
        .clone();
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("a runtime starts");
    // One at a time: a slow answer has to exhaust a budget measured in real
    // seconds, which contention could stretch.
    let results: Vec<(String, Value, Value)> = runtime.block_on(async {
        let mut results = Vec::new();
        for (index, case) in cases.iter().enumerate() {
            let home = scratch.path().join(format!("probe-{index}"));
            let observed = run_probe_case(case, &home).await;
            results.push((
                format!(
                    "availabilityProbes/{}",
                    case["name"].as_str().expect("a name")
                ),
                case["observed"].clone(),
                observed,
            ));
        }
        results
    });
    let mut unexplained = Vec::new();
    let mut conformant = 0;
    for (name, reference, port) in &results {
        let before = unexplained.len();
        compare(name, reference, port, &mut unexplained);
        conformant += usize::from(unexplained.len() == before);
    }
    println!(
        "fast-model probe parity: {conformant}/{} cases conformant",
        results.len()
    );
    assert!(
        unexplained.is_empty(),
        "{} departures from the corpus:\n{}",
        unexplained.len(),
        unexplained.join("\n")
    );
}

/// What the oracle's `RecordingTelemetry` keeps of each event the client
/// would have sent.
#[derive(Default)]
struct RecordingTelemetry(Mutex<Vec<Value>>);

impl ClientTelemetry for RecordingTelemetry {
    fn record_client_event(
        &self,
        _name: &str,
        _properties: serde_json::Map<String, Value>,
        _session_id: Option<&str>,
        _correlate_last_request: bool,
    ) {
    }

    fn record(&self, record: &TelemetryRecord, _session_id: Option<&str>) {
        let properties = record
            .attributes(None)
            .expect("a utility record has attributes")
            .into_properties();
        self.0.lock().expect("events lock").push(json!({
            "event": record.event().event_name(),
            "properties": properties,
            "correlationId": null,
        }));
    }
}

fn launch(value: &Value) -> Option<LaunchContext> {
    let field = |name: &str| value[name].as_str().unwrap_or_default().to_owned();
    value.is_object().then(|| LaunchContext {
        agent_entrypoint: field("agent_entrypoint"),
        agent_version: field("agent_version"),
        client_name: field("client_name"),
        client_version: field("client_version"),
        terminal_emulator: value["terminal_emulator"].as_str().map(str::to_owned),
    })
}

/// The label a case asks for: its feature's, the one it names, or the
/// reference's default.
fn call_type(case: &Value) -> TelemetryCallType {
    if let Some(feature) = feature(&case["feature"]) {
        return utility::feature_call_type(feature);
    }
    match case["callType"].as_str() {
        Some("worktree_title") => TelemetryCallType::WorktreeTitle,
        Some(other) => panic!("an unknown call type {other}"),
        None => TelemetryCallType::SecondaryCall,
    }
}

async fn run_completion_case(case: &Value, home: &Path) -> Value {
    let stand_in =
        StandIn::start(case["responses"].as_array().map_or(&[][..], Vec::as_slice)).await;
    let substitutions = [("$BASE", stand_in.base.as_str())];
    let providers: Vec<ProviderConfig> = case["providers"]
        .as_array()
        .expect("providers")
        .iter()
        .map(|entry| provider(&substitute(entry, &substitutions)))
        .collect();
    let routing = routing(case, providers);
    let credentials: Arc<dyn Credentials> = Arc::new(MapCredentials(
        case["env"]
            .as_object()
            .expect("an environment")
            .iter()
            .map(|(name, value)| (name.clone(), value.as_str().unwrap_or_default().to_owned()))
            .collect(),
    ));
    let availability = ModelAvailability::new(home.join(CACHE_FILE), Arc::new(HandClock::new()));
    let context = BackendContext {
        api: ApiSettings::default(),
        clock: Arc::new(FakeClock::new()),
        credentials: Arc::clone(&credentials),
        vertex: Arc::new(FakeVertex(stand_in.base.clone())),
    };
    let telemetry = case["telemetry"]
        .as_bool()
        .expect("a telemetry switch")
        .then(RecordingTelemetry::default);
    let launch = launch(&case["launch"]);
    let selection = utility::select(
        &routing,
        credentials.as_ref(),
        &availability,
        feature(&case["feature"]),
    )
    .expect("every case selects a model");
    let request = utility::UtilityRequest {
        system_prompt: "Answer with a short label.",
        user_content: "Résumé of the café session 🚀",
        max_tokens: 24,
        request_timeout: Duration::from_secs(5),
        retry_budget: Duration::ZERO,
        skip_if_no_key: case["skipIfNoKey"].as_bool().expect("a skip switch"),
        call_type: call_type(case),
        attribution: utility::Attribution {
            launch: launch.as_ref(),
            session_id: case["session"].as_str(),
            telemetry: telemetry
                .as_ref()
                .map(|telemetry| telemetry as &dyn ClientTelemetry),
        },
    };
    let content = match utility::complete(&selection, &context, &request).await {
        Ok(content) => json!(content),
        Err(error) => json!({"error": error.to_string()}),
    };
    let observed = json!({
        "content": content,
        "requests": normalize_platform(stand_in.requests()),
        "telemetry": telemetry.map(|telemetry| {
            Value::Array(telemetry.0.into_inner().expect("events lock"))
        }),
    });
    substitute(
        &observed,
        &[
            (stand_in.base.as_str(), "$BASE"),
            (env!("CARGO_PKG_VERSION"), "<version>"),
        ],
    )
}

#[test]
fn every_utility_completion_is_labeled_and_attributed_as_the_corpus_records() {
    let corpus: Value = serde_json::from_str(CORPUS).expect("the corpus parses");
    let cases = corpus["utilityCompletions"]
        .as_array()
        .expect("the corpus holds utility completions")
        .clone();
    let scratch = tempfile::tempdir().expect("a scratch directory");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("a runtime starts");
    let mut unexplained = Vec::new();
    for (index, case) in cases.iter().enumerate() {
        let home = scratch.path().join(format!("completion-{index}"));
        let port = runtime.block_on(run_completion_case(case, &home));
        let name = format!(
            "utilityCompletions/{}",
            case["name"].as_str().expect("a name")
        );
        compare(&name, &case["observed"], &port, &mut unexplained);
    }
    assert!(
        unexplained.is_empty(),
        "utility completions depart from the corpus:\n{}",
        unexplained.join("\n")
    );
    println!(
        "utility completions: {}/{} conformant",
        cases.len(),
        cases.len()
    );
}

/// Worktree naming reaches the backend through the completion provider rather
/// than [`utility::complete`]; what it sends is held to the request the
/// `worktree-title` case recorded, apart from the prompt each side authors.
#[test]
fn a_worktree_name_request_is_labeled_and_attributed_as_the_corpus_records() {
    let corpus: Value = serde_json::from_str(CORPUS).expect("the corpus parses");
    let case = corpus["utilityCompletions"]
        .as_array()
        .expect("the corpus holds utility completions")
        .iter()
        .find(|case| case["name"] == "worktree-title")
        .expect("the worktree-title case")
        .clone();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("a runtime starts");
    let port = runtime.block_on(async {
        let stand_in =
            StandIn::start(case["responses"].as_array().map_or(&[][..], Vec::as_slice)).await;
        let substitutions = [("$BASE", stand_in.base.as_str())];
        let providers: Vec<ProviderConfig> = case["providers"]
            .as_array()
            .expect("providers")
            .iter()
            .map(|entry| provider(&substitute(entry, &substitutions)))
            .collect();
        let routing = routing(&case, providers);
        let credentials: Arc<dyn Credentials> = Arc::new(MapCredentials(
            [("ORACLE_API_KEY".to_owned(), KEY.to_owned())].into(),
        ));
        let scratch = tempfile::tempdir().expect("a scratch directory");
        let availability =
            ModelAvailability::new(scratch.path().join(CACHE_FILE), Arc::new(HandClock::new()));
        let selection = utility::select(&routing, credentials.as_ref(), &availability, None)
            .expect("the case selects a model");
        let context = BackendContext {
            api: ApiSettings::default(),
            clock: Arc::new(FakeClock::new()),
            credentials,
            vertex: Arc::new(FakeVertex(stand_in.base.clone())),
        };
        let name = selection.model.name.clone();
        let completion = super::completion::LlmCompletion::new(
            selection.provider,
            vec![selection.model],
            name,
            context,
        )
        .expect("a completion provider");
        let launch = launch(&case["launch"]);
        let answer = crate::worktree::naming_model::suggest_worktree_name(
            Some("name this"),
            Some(&completion),
            launch.as_ref(),
        )
        .await;
        assert_eq!(answer.as_deref(), case["observed"]["content"].as_str());
        substitute(
            &Value::Array(normalize_platform(stand_in.requests())),
            &[
                (stand_in.base.as_str(), "$BASE"),
                (env!("CARGO_PKG_VERSION"), "<version>"),
            ],
        )
    });
    let recorded = &case["observed"]["requests"][0];
    let sent = &port[0];
    let mut unexplained = Vec::new();
    for pointer in [
        "/method",
        "/path",
        "/headers",
        "/body/model",
        "/body/max_tokens",
        "/body/temperature",
        "/body/metadata",
    ] {
        compare(
            &format!("worktree name {pointer}"),
            recorded.pointer(pointer).unwrap_or(&Value::Null),
            sent.pointer(pointer).unwrap_or(&Value::Null),
            &mut unexplained,
        );
    }
    assert!(
        unexplained.is_empty(),
        "the worktree name request departs from the corpus:\n{}",
        unexplained.join("\n")
    );
}
