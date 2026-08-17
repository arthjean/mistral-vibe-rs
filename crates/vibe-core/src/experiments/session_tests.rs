//! The two gates, the credential rule, the attributes and what a resolved
//! lookup leaves behind.

use std::path::Path;
use std::sync::Arc;
use toml::Table;

use crate::config::registry::default_document;
use crate::config::{ConfigPaths, LayeredConfig};
use crate::identity::recorder::RecordingResolver;
use crate::telemetry::LaunchContext;

use super::client::RemoteEvalClient;
use super::manager::{ExperimentManager, hash_api_key};
use super::models::{EvalResponse, ExperimentAttributes};
use super::recorder::{Outcome, RecordingSink, RecordingTransport};
use super::session::{
    EXPERIMENT_IDENTITY_TIMEOUT, build_attributes, experiments_allowed,
    hydrate_experiments_from_session, initialize_experiments, mistral_provider_and_api_key,
};

const ORACLE_URL: &str = "https://experiments.example.test/api/eval/sdk-key";
const ORACLE_VARIABLE: &str = "ORACLE_MISTRAL_KEY";
const ORACLE_KEY: &str = "oracle-mistral-sentinel";
const ORACLE_API_BASE: &str = "https://api.mistral.ai/v1";

/// One forced system-prompt feature, which is what every successful lookup in
/// these tests answers.
const ORACLE_RESPONSE: &str = r#"{"features": {"vibe_cli_system_prompt": {"defaultValue": "cli",
    "rules": [{"force": "tests", "tracks": [{"experiment": {"key": "vibe_cli_system_prompt"},
    "result": {"key": "1", "variationId": 1, "inExperiment": true}}]}]}}}"#;

const MISTRAL_PROVIDER: &str = r#"
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

const THIRD_PARTY_PROVIDER: &str = r#"
[[providers]]
name = "third-party"
api_base = "https://third-party.example.test/v1"
api_key_env_var = "ORACLE_THIRD_PARTY_KEY"

[[models]]
name = "oracle-model"
provider = "third-party"
alias = "oracle"
"#;

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a current-thread runtime builds")
}

/// The merged document one authored file composes, loaded through the same
/// layering a session loads: the shipped defaults under the operator's file.
fn effective(document: &str) -> Table {
    let temporary = tempfile::tempdir().expect("a session scratch directory");
    compose(temporary.path(), document)
}

fn compose(root: &Path, document: &str) -> Table {
    let home = root.join("home/.vibe");
    let working = root.join("project");
    std::fs::create_dir_all(&home).expect("the home directory");
    std::fs::create_dir_all(&working).expect("the project directory");
    std::fs::write(home.join("config.toml"), document).expect("the document writes");
    LayeredConfig::new(
        ConfigPaths {
            vibe_home: home,
            working_directory: working,
        },
        default_document(),
    )
    .load()
    .expect("the authored document composes")
    .effective
}

fn credentials(name: &str) -> Option<String> {
    (name == ORACLE_VARIABLE).then(|| ORACLE_KEY.to_owned())
}

struct Run {
    refreshed: bool,
    eval_requests: usize,
    identity_calls: Vec<crate::identity::recorder::RecordedResolution>,
    persisted: usize,
    attributes: Option<ExperimentAttributes>,
    manager: ExperimentManager,
}

/// One initialization, driven over a recorder standing where the connection
/// would be.
fn initialize(document: &str, outcome: Outcome, organization: Option<&'static str>) -> Run {
    let effective = effective(document);
    let transport = Arc::new(RecordingTransport::new(outcome));
    let mut manager = ExperimentManager::new(RemoteEvalClient::with_transport(
        Some(ORACLE_URL.to_owned()),
        Arc::clone(&transport) as _,
    ));
    let resolver = RecordingResolver::answering(organization, ORACLE_KEY);
    let sink = RecordingSink::default();
    let refreshed = runtime().block_on(initialize_experiments(
        &effective,
        &credentials,
        &mut manager,
        None,
        &resolver,
        &sink,
    ));
    let attributes = transport.requests().first().and_then(|request| {
        request
            .payload
            .get("attributes")
            .cloned()
            .and_then(|value| serde_json::from_value(value).ok())
    });
    Run {
        refreshed,
        eval_requests: transport.request_count(),
        identity_calls: resolver.calls(),
        persisted: sink.count(),
        attributes,
        manager,
    }
}

#[test]
fn a_configured_session_looks_the_rollout_up_once_and_persists_what_it_resolved() {
    let run = initialize(
        MISTRAL_PROVIDER,
        Outcome::ok(ORACLE_RESPONSE),
        Some("oracle-organization"),
    );
    assert!(run.refreshed);
    assert_eq!(run.eval_requests, 1);
    assert_eq!(run.persisted, 1);
    assert_eq!(run.identity_calls.len(), 1);
    let call = &run.identity_calls[0];
    assert_eq!(call.base_url, ORACLE_API_BASE);
    assert_eq!(call.timeout, Some(EXPERIMENT_IDENTITY_TIMEOUT));
    assert!(
        call.carries_key,
        "the identity read authenticates as the caller"
    );
    let attributes = run.attributes.expect("the request carried its attributes");
    assert_eq!(
        attributes.organization_id.as_deref(),
        Some("oracle-organization")
    );
    assert_eq!(attributes.user_id, hash_api_key(ORACLE_KEY));
    assert_ne!(attributes.user_id, ORACLE_KEY);
    assert_eq!(
        run.manager.variant(super::ExperimentName::SystemPrompt),
        "tests"
    );
}

#[test]
fn either_gate_stops_every_request_before_it_is_built() {
    for document in [
        format!("enable_telemetry = false\n{MISTRAL_PROVIDER}"),
        format!("[experiments]\nenable = false\n{MISTRAL_PROVIDER}"),
        format!("enable_telemetry = false\n[experiments]\nenable = false\n{MISTRAL_PROVIDER}"),
    ] {
        let run = initialize(&document, Outcome::ok(ORACLE_RESPONSE), Some("oracle"));
        assert!(!run.refreshed, "{document}");
        assert_eq!(run.eval_requests, 0, "{document}");
        assert_eq!(run.identity_calls.len(), 0, "{document}");
        assert_eq!(run.persisted, 0, "{document}");
    }
}

#[test]
fn no_mistral_provider_and_no_resolvable_key_both_stop_the_lookup() {
    for document in [
        THIRD_PARTY_PROVIDER.to_owned(),
        MISTRAL_PROVIDER.replace(ORACLE_VARIABLE, "ORACLE_ABSENT_KEY"),
    ] {
        let run = initialize(&document, Outcome::ok(ORACLE_RESPONSE), Some("oracle"));
        assert!(!run.refreshed, "{document}");
        assert_eq!(run.eval_requests, 0, "{document}");
        assert_eq!(run.identity_calls.len(), 0, "{document}");
    }
}

#[test]
fn a_failed_lookup_persists_nothing_and_asks_for_no_refresh() {
    for outcome in [
        Outcome::Failure,
        Outcome::Answer {
            status: 500,
            body: String::new(),
        },
        Outcome::ok("not json"),
    ] {
        let run = initialize(MISTRAL_PROVIDER, outcome, None);
        assert!(!run.refreshed);
        assert_eq!(run.eval_requests, 1);
        assert_eq!(run.persisted, 0);
        assert_eq!(
            run.manager.variant(super::ExperimentName::SystemPrompt),
            "cli",
            "the session keeps its declared default"
        );
    }
}

/// An empty answer is still an answer: the reference persists it and reports a
/// refresh, because the manager took a state in.
#[test]
fn an_empty_response_is_a_resolved_state() {
    let run = initialize(MISTRAL_PROVIDER, Outcome::ok(r#"{"features": {}}"#), None);
    assert!(run.refreshed);
    assert_eq!(run.persisted, 1);
    let attributes = run.attributes.expect("the request carried its attributes");
    assert_eq!(attributes.organization_id, None);
}

#[test]
fn an_unreachable_identity_costs_the_attribute_and_nothing_else() {
    let run = initialize(MISTRAL_PROVIDER, Outcome::ok(ORACLE_RESPONSE), None);
    assert!(run.refreshed);
    assert_eq!(run.identity_calls.len(), 1);
    let attributes = run.attributes.expect("the request carried its attributes");
    assert_eq!(attributes.organization_id, None);
}

#[test]
fn hydration_reads_the_same_two_gates_and_issues_nothing() {
    let state: EvalResponse =
        serde_json::from_str(ORACLE_RESPONSE).expect("the authored response validates");
    let transport = Arc::new(RecordingTransport::answering(ORACLE_RESPONSE));
    for (document, expected) in [
        (MISTRAL_PROVIDER.to_owned(), true),
        (
            format!("enable_telemetry = false\n{MISTRAL_PROVIDER}"),
            false,
        ),
        (
            format!("[experiments]\nenable = false\n{MISTRAL_PROVIDER}"),
            false,
        ),
    ] {
        let mut manager = ExperimentManager::new(RemoteEvalClient::with_transport(
            Some(ORACLE_URL.to_owned()),
            Arc::clone(&transport) as _,
        ));
        assert_eq!(
            hydrate_experiments_from_session(
                &effective(&document),
                &mut manager,
                Some(state.clone())
            ),
            expected,
            "{document}"
        );
    }
    assert_eq!(transport.request_count(), 0, "a hydration issues nothing");
}

#[test]
fn a_session_carrying_no_state_hydrates_nothing() {
    let mut manager = ExperimentManager::new(RemoteEvalClient::with_url(None));
    assert!(!hydrate_experiments_from_session(
        &effective(MISTRAL_PROVIDER),
        &mut manager,
        None
    ));
    assert_eq!(manager.export_state(), None);
}

#[test]
fn the_gates_default_to_open_on_a_document_that_names_neither() {
    assert!(experiments_allowed(&Table::new()));
    assert!(experiments_allowed(&effective(MISTRAL_PROVIDER)));
}

/// The rule that keeps a third-party credential off a Mistral-controlled
/// endpoint, proven with a resolver that would happily hand one over: the
/// third-party variable resolves, and the lookup still refuses to carry it.
#[test]
fn a_third_party_credential_never_resolves_for_a_mistral_endpoint() {
    let generous = |name: &str| match name {
        ORACLE_VARIABLE => Some(ORACLE_KEY.to_owned()),
        "ORACLE_THIRD_PARTY_KEY" => Some("oracle-third-party-sentinel".to_owned()),
        _ => None,
    };
    let resolved = mistral_provider_and_api_key(&effective(THIRD_PARTY_PROVIDER), &generous);
    assert!(
        resolved
            .as_ref()
            .is_none_or(|(_, key)| key != "oracle-third-party-sentinel"),
        "the third-party credential never leaves for a Mistral endpoint: {resolved:?}"
    );
    assert_eq!(
        mistral_provider_and_api_key(&effective(MISTRAL_PROVIDER), &credentials),
        Some((ORACLE_API_BASE.to_owned(), ORACLE_KEY.to_owned()))
    );
    // A Mistral provider whose variable resolves to nothing stops the lookup
    // exactly as an absent provider does.
    assert_eq!(
        mistral_provider_and_api_key(
            &effective(&MISTRAL_PROVIDER.replace(ORACLE_VARIABLE, "ORACLE_ABSENT_KEY")),
            &|_| None
        ),
        None
    );
}

#[test]
fn the_attributes_follow_the_launch_context_and_the_prompt_the_document_names() {
    let document = effective(MISTRAL_PROVIDER);
    let bare = build_attributes(&document, ORACLE_KEY, None, None);
    assert_eq!(bare.entrypoint, "unknown");
    assert_eq!(bare.agent_version, crate::telemetry::version());
    assert_eq!(bare.client_name, None);
    assert_eq!(bare.client_version, None);
    assert_eq!(bare.terminal_emulator, None);
    assert!(!bare.custom_system_prompt);

    let launch = LaunchContext {
        agent_entrypoint: "acp".to_owned(),
        agent_version: "9.9.9".to_owned(),
        client_name: "zed".to_owned(),
        client_version: "0.1.0".to_owned(),
        terminal_emulator: Some("cursor".to_owned()),
    };
    let launched = build_attributes(
        &document,
        ORACLE_KEY,
        Some(&launch),
        Some("oracle-organization".to_owned()),
    );
    assert_eq!(launched.entrypoint, "acp");
    assert_eq!(launched.agent_version, "9.9.9");
    assert_eq!(launched.client_name.as_deref(), Some("zed"));
    assert_eq!(launched.client_version.as_deref(), Some("0.1.0"));
    assert_eq!(launched.terminal_emulator.as_deref(), Some("cursor"));
    assert_eq!(
        launched.organization_id.as_deref(),
        Some("oracle-organization")
    );

    let custom = effective(&format!("system_prompt_id = \"lean\"\n{MISTRAL_PROVIDER}"));
    assert!(build_attributes(&custom, ORACLE_KEY, None, None).custom_system_prompt);
}
