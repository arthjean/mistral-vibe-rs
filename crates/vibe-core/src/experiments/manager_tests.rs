//! The bucketing key, the known-key filter, the two variant readers and the
//! split between what configuration may read and what telemetry may report.

use std::sync::Arc;

use super::ExperimentName;
use super::client::RemoteEvalClient;
use super::manager::{BUCKETING_KEY_LENGTH, ExperimentManager, hash_api_key};
use super::models::{EvalResponse, ExperimentAttributes};
use super::recorder::{Outcome, RecordingTransport};

fn hydrated(document: &str) -> ExperimentManager {
    let mut manager = ExperimentManager::new(RemoteEvalClient::with_url(None));
    manager.hydrate(serde_json::from_str::<EvalResponse>(document).expect("the response parses"));
    manager
}

fn attributes() -> ExperimentAttributes {
    ExperimentAttributes {
        user_id: hash_api_key("oracle-mistral-sentinel"),
        entrypoint: "cli".to_owned(),
        agent_version: "9.9.9".to_owned(),
        client_name: None,
        client_version: None,
        os: "linux".to_owned(),
        terminal_emulator: None,
        custom_system_prompt: false,
        organization_id: None,
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("a current-thread runtime builds")
}

#[test]
fn the_bucketing_key_is_a_truncated_digest_that_is_stable_and_never_the_key() {
    let key = hash_api_key("oracle-mistral-sentinel");
    assert_eq!(key.len(), BUCKETING_KEY_LENGTH);
    assert!(key.chars().all(|character| character.is_ascii_hexdigit()));
    assert!(key.chars().all(|character| !character.is_ascii_uppercase()));
    assert_eq!(key, hash_api_key("oracle-mistral-sentinel"), "stable");
    assert_ne!(key, hash_api_key("oracle-second-sentinel"), "per key");
    assert!(
        !key.contains("oracle"),
        "the credential is not recoverable from the digest"
    );
}

#[test]
fn an_unread_manager_answers_this_builds_own_defaults() {
    let manager = ExperimentManager::new(RemoteEvalClient::with_url(None));
    assert!(manager.export_state().is_none());
    for name in ExperimentName::ALL {
        assert_eq!(manager.variant_or_none(name), None);
        assert_eq!(manager.variant(name), name.default_variant());
    }
    assert!(manager.assignments().is_empty());
    assert!(manager.config_variants().is_empty());
}

#[test]
fn a_feature_this_build_does_not_know_is_dropped_on_the_way_in() {
    let manager = hydrated(
        r#"{"features": {
            "vibe_cli_unknown_rollout": {"defaultValue": "x", "rules": [{"force": "y"}]},
            "vibe_cli_system_prompt": {"defaultValue": "cli", "rules": [{"force": "tests"}]}
        }}"#,
    );
    let state = manager.export_state().expect("the state is kept");
    assert_eq!(
        state.features.keys().collect::<Vec<_>>(),
        ["vibe_cli_system_prompt"]
    );
    assert_eq!(manager.variant(ExperimentName::SystemPrompt), "tests");
}

#[test]
fn an_object_or_array_variant_answers_its_json_serialization() {
    let manager = hydrated(
        r#"{"features": {"vibe_cli_default_routing_model": {"defaultValue": null, "rules": [
            {"force": {"active_model": "alias", "model_config": {"name": "n", "provider": "p"}}}
        ]}}}"#,
    );
    assert_eq!(
        manager.variant(ExperimentName::CliModelRouting),
        r#"{"active_model": "alias", "model_config": {"name": "n", "provider": "p"}}"#,
        "the payload reaches the caller in the order the wire carried"
    );

    let array = hydrated(
        r#"{"features": {"vibe_cli_system_prompt": {"defaultValue": null, "rules": [{"force": ["cli", "lean"]}]}}}"#,
    );
    assert_eq!(
        array.variant(ExperimentName::SystemPrompt),
        r#"["cli", "lean"]"#
    );

    let scalar = hydrated(
        r#"{"features": {"vibe_cli_managed_shell_tools": {"defaultValue": null, "rules": [{"force": true}]}}}"#,
    );
    assert_eq!(scalar.variant(ExperimentName::ManagedShellTools), "true");
}

#[test]
fn a_feature_resolving_to_nothing_falls_back_to_the_default_variant() {
    let manager = hydrated(
        r#"{"features": {"vibe_cli_system_prompt": {"defaultValue": null, "rules": [{"tracks": []}]}}}"#,
    );
    assert_eq!(manager.variant_or_none(ExperimentName::SystemPrompt), None);
    assert_eq!(manager.variant(ExperimentName::SystemPrompt), "cli");
    assert_eq!(
        manager.variant(ExperimentName::CliModelRouting),
        "{}",
        "a feature the response never carried falls back too"
    );
}

#[test]
fn a_force_reaches_configuration_and_only_a_confirmed_track_reaches_telemetry() {
    let forced = hydrated(
        r#"{"features": {"vibe_cli_system_prompt": {"defaultValue": "cli", "rules": [{"force": "tests"}]}}}"#,
    );
    assert!(
        forced.assignments().is_empty(),
        "a force is not an enrollment"
    );
    assert_eq!(
        forced
            .config_variants()
            .get("vibe_cli_system_prompt")
            .map(String::as_str),
        Some("tests"),
        "but configuration still honors it"
    );

    for flag in [r#""inExperiment": false, "#, ""] {
        let unconfirmed = hydrated(&format!(
            r#"{{"features": {{"vibe_cli_system_prompt": {{"defaultValue": "cli", "rules": [
                {{"force": "tests", "tracks": [{{"experiment": {{"key": "vibe_cli_system_prompt"}}, "result": {{{flag}"key": "1", "variationId": 1}}}}]}}
            ]}}}}}}"#
        ));
        assert!(unconfirmed.assignments().is_empty(), "flag {flag:?}");
        assert_eq!(unconfirmed.config_variants().len(), 1, "flag {flag:?}");
    }

    let confirmed = hydrated(
        r#"{"features": {"vibe_cli_system_prompt": {"defaultValue": "cli", "rules": [
            {"force": "tests", "tracks": [{"experiment": {"key": "vibe_cli_system_prompt"}, "result": {"inExperiment": true, "key": "1", "variationId": 1}}]}
        ]}}}"#,
    );
    assert_eq!(
        confirmed
            .assignments()
            .get("vibe_cli_system_prompt")
            .map(String::as_str),
        Some("tests")
    );
    assert_eq!(confirmed.assignments(), confirmed.config_variants());
}

#[test]
fn the_label_falls_back_four_levels_and_an_exhausted_one_is_not_reported() {
    let track = |result: &str| {
        format!(
            r#"{{"features": {{"vibe_cli_system_prompt": {{"defaultValue": {{DEFAULT}}, "rules": [
                {{"tracks": [{{"experiment": {{"key": "vibe_cli_system_prompt"}}, "result": {result}}}]}}
            ]}}}}}}"#
        )
    };
    let with_default = |document: String, default: &str| document.replace("{DEFAULT}", default);

    // The value the track carried wins.
    let from_track = hydrated(&with_default(
        track(r#"{"inExperiment": true, "key": "1", "value": "from-track", "variationId": 1}"#),
        r#""cli""#,
    ));
    assert_eq!(
        from_track
            .assignments()
            .get("vibe_cli_system_prompt")
            .map(String::as_str),
        Some("from-track")
    );

    // An object value is serialized rather than dropped.
    let object = hydrated(&with_default(
        track(r#"{"inExperiment": true, "value": {"variant": "managed"}}"#),
        "null",
    ));
    assert_eq!(
        object
            .assignments()
            .get("vibe_cli_system_prompt")
            .map(String::as_str),
        Some(r#"{"variant": "managed"}"#)
    );

    // Then the value the feature resolves to.
    let resolved = hydrated(&with_default(
        track(r#"{"inExperiment": true, "key": "1", "variationId": 1}"#),
        r#""cli""#,
    ));
    assert_eq!(
        resolved
            .assignments()
            .get("vibe_cli_system_prompt")
            .map(String::as_str),
        Some("cli")
    );

    // Then the key of the result.
    let key = hydrated(&with_default(
        track(r#"{"inExperiment": true, "key": "control"}"#),
        "null",
    ));
    assert_eq!(
        key.assignments()
            .get("vibe_cli_system_prompt")
            .map(String::as_str),
        Some("control")
    );

    // Then the variation number, zero included.
    for (result, expected) in [
        (r#"{"inExperiment": true, "variationId": 3}"#, "3"),
        (r#"{"inExperiment": true, "variationId": 0}"#, "0"),
    ] {
        let variation = hydrated(&with_default(track(result), "null"));
        assert_eq!(
            variation
                .assignments()
                .get("vibe_cli_system_prompt")
                .map(String::as_str),
            Some(expected)
        );
    }

    // And an exhausted fallback is reported as nothing at all.
    let exhausted = hydrated(&with_default(track(r#"{"inExperiment": true}"#), "null"));
    assert!(exhausted.assignments().is_empty());
}

#[test]
fn the_last_confirmed_track_of_a_feature_wins() {
    let manager = hydrated(
        r#"{"features": {"vibe_cli_system_prompt": {"defaultValue": null, "rules": [
            {"tracks": [
                {"experiment": {"key": "vibe_cli_system_prompt"}, "result": {"inExperiment": true, "value": "first"}},
                {"experiment": {"key": "vibe_cli_system_prompt"}, "result": {"inExperiment": true, "value": "second"}}
            ]}
        ]}}}"#,
    );
    assert_eq!(
        manager
            .assignments()
            .get("vibe_cli_system_prompt")
            .map(String::as_str),
        Some("second")
    );
}

#[test]
fn the_experiment_key_of_a_track_does_not_have_to_be_the_feature_key() {
    let manager = hydrated(
        r#"{"features": {"vibe_cli_managed_shell_tools": {"defaultValue": "legacy", "rules": [
            {"force": "managed", "tracks": [{"experiment": {"key": "some_other_experiment"}, "result": {"inExperiment": true, "key": "1", "variationId": 1}}]}
        ]}}}"#,
    );
    assert_eq!(
        manager
            .assignments()
            .get("vibe_cli_managed_shell_tools")
            .map(String::as_str),
        Some("managed"),
        "the feature key is what an exposure is reported under"
    );
}

#[test]
fn a_second_initialization_replaces_the_previous_state() {
    let runtime = runtime();
    let first = Arc::new(RecordingTransport::answering(
        r#"{"features": {"vibe_cli_system_prompt": {"defaultValue": "cli", "rules": [{"force": "tests"}]}}}"#,
    ));
    let mut manager = ExperimentManager::new(RemoteEvalClient::with_transport(
        Some("https://experiments.example.test/api/eval/sdk-key".to_owned()),
        Arc::clone(&first) as _,
    ));
    runtime.block_on(manager.initialize(&attributes()));
    assert_eq!(manager.variant(ExperimentName::SystemPrompt), "tests");

    // A second response replaces rather than merges: the first feature is gone.
    manager.hydrate(
        serde_json::from_str(
            r#"{"features": {"vibe_cli_managed_shell_tools": {"defaultValue": "legacy", "rules": [{"force": "managed"}]}}}"#,
        )
        .expect("the second response parses"),
    );
    assert_eq!(manager.variant(ExperimentName::SystemPrompt), "cli");
    assert_eq!(
        manager.variant(ExperimentName::ManagedShellTools),
        "managed"
    );
}

#[test]
fn a_failed_lookup_leaves_the_previous_state_untouched() {
    let runtime = runtime();
    let failing = Arc::new(RecordingTransport::new(Outcome::Failure));
    let mut manager = ExperimentManager::new(RemoteEvalClient::with_transport(
        Some("https://experiments.example.test/api/eval/sdk-key".to_owned()),
        Arc::clone(&failing) as _,
    ));
    manager.hydrate(
        serde_json::from_str(
            r#"{"features": {"vibe_cli_system_prompt": {"defaultValue": "cli", "rules": [{"force": "tests"}]}}}"#,
        )
        .expect("the seed response parses"),
    );
    runtime.block_on(manager.initialize(&attributes()));
    assert_eq!(
        manager.variant(ExperimentName::SystemPrompt),
        "tests",
        "a failed refresh cannot empty a session that had already resolved"
    );
    assert_eq!(failing.request_count(), 1);

    runtime.block_on(manager.close());
    assert_eq!(failing.closes(), 1);
}
