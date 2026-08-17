//! What the eval models accept, what they ignore, and what a feature resolves
//! to.

use super::json::JsonValue;
use super::models::{EvalResponse, ExperimentAttributes, FeatureDefinition};

fn response(text: &str) -> EvalResponse {
    serde_json::from_str(text).expect("the authored response parses")
}

fn feature(text: &str) -> FeatureDefinition {
    serde_json::from_str(text).expect("the authored feature parses")
}

#[test]
fn a_response_ignores_every_field_the_models_do_not_declare() {
    // The proxy returns a scrubbed `experiments` block beside `features`, and a
    // rule keeps whatever bookkeeping its proxy version attached. Neither may
    // fail the lookup.
    let decoded = response(
        r#"{
            "features": {
                "vibe_cli_system_prompt": {
                    "defaultValue": "cli",
                    "scrubbed": true,
                    "rules": [
                        {"force": "tests", "condition": {"country": "FR"}, "coverage": 0.5}
                    ]
                }
            },
            "experiments": [{"key": "unused"}],
            "dateUpdated": "2026-08-17"
        }"#,
    );
    let resolved = decoded
        .features
        .get("vibe_cli_system_prompt")
        .expect("the known feature survives")
        .resolved_value()
        .clone();
    assert_eq!(resolved, JsonValue::String("tests".to_owned()));
}

#[test]
fn a_forced_rule_wins_over_the_default_value() {
    let definition = feature(r#"{"defaultValue": "cli", "rules": [{"force": "tests"}]}"#);
    assert_eq!(
        *definition.resolved_value(),
        JsonValue::String("tests".to_owned())
    );
    // The first rule carrying a force wins, not the first rule.
    let later = feature(r#"{"defaultValue": "cli", "rules": [{}, {"force": "tests"}]}"#);
    assert_eq!(
        *later.resolved_value(),
        JsonValue::String("tests".to_owned())
    );
}

#[test]
fn a_rule_without_a_force_falls_through_to_the_default_value() {
    let none = feature(r#"{"defaultValue": "cli", "rules": [{}, {"tracks": []}]}"#);
    assert_eq!(*none.resolved_value(), JsonValue::String("cli".to_owned()));
    // A rule forcing null is a rule without a force, upstream as here.
    let forced_null = feature(r#"{"defaultValue": "cli", "rules": [{"force": null}]}"#);
    assert_eq!(
        *forced_null.resolved_value(),
        JsonValue::String("cli".to_owned())
    );
    // And a null default resolves to null rather than to an error.
    let null_default = feature(r#"{"defaultValue": null, "rules": []}"#);
    assert!(null_default.resolved_value().is_null());
    // A feature carrying no rules key at all is a feature with no rules.
    let bare = feature(r#"{"defaultValue": "cli"}"#);
    assert_eq!(*bare.resolved_value(), JsonValue::String("cli".to_owned()));
}

#[test]
fn a_falsy_forced_value_is_still_a_force() {
    for (document, expected) in [
        (
            r#"{"defaultValue": "cli", "rules": [{"force": ""}]}"#,
            JsonValue::String(String::new()),
        ),
        (
            r#"{"defaultValue": "cli", "rules": [{"force": false}]}"#,
            JsonValue::Bool(false),
        ),
    ] {
        assert_eq!(*feature(document).resolved_value(), expected);
    }
    let zero = feature(r#"{"defaultValue": "cli", "rules": [{"force": 0}]}"#);
    assert_eq!(zero.resolved_value().python_json(), "0");
}

#[test]
fn a_malformed_response_is_an_error_rather_than_a_panic() {
    for document in [
        r#"{"features": 17}"#,
        r#"{"features": {"a": 17}}"#,
        r#"{"features": {"a": {"rules": [{"tracks": [{"result": {}}]}]}}}"#,
        "<html>nope</html>",
        "",
    ] {
        assert!(
            serde_json::from_str::<EvalResponse>(document).is_err(),
            "{document} is not a response this build accepts"
        );
    }
    // An empty document is a response with no features rather than a failure.
    assert_eq!(response("{}"), EvalResponse::default());
}

#[test]
fn the_attributes_omit_what_is_absent_and_keep_what_is_false() {
    let attributes = ExperimentAttributes {
        user_id: "0123456789abcdef0123456789abcdef".to_owned(),
        entrypoint: "cli".to_owned(),
        agent_version: "9.9.9".to_owned(),
        client_name: None,
        client_version: None,
        os: "linux".to_owned(),
        terminal_emulator: None,
        custom_system_prompt: false,
        organization_id: None,
    };
    let encoded = serde_json::to_value(&attributes).expect("the attributes serialize");
    let object = encoded.as_object().expect("the attributes are an object");
    assert_eq!(
        object.keys().map(String::as_str).collect::<Vec<_>>(),
        [
            "agent_version",
            "custom_system_prompt",
            "entrypoint",
            "os",
            "userId"
        ],
        "an absent optional is omitted and a false boolean is not"
    );
    assert_eq!(
        object.get("userId").and_then(serde_json::Value::as_str),
        Some("0123456789abcdef0123456789abcdef")
    );
}

#[test]
fn the_attributes_spell_the_two_camel_case_names_the_proxy_expects() {
    let attributes = ExperimentAttributes {
        user_id: "digest".to_owned(),
        entrypoint: "acp".to_owned(),
        agent_version: "9.9.9".to_owned(),
        client_name: Some("oracle-client".to_owned()),
        client_version: Some("1.2.3".to_owned()),
        os: "linux".to_owned(),
        terminal_emulator: Some("vscode".to_owned()),
        custom_system_prompt: true,
        organization_id: Some("oracle-organization".to_owned()),
    };
    let encoded = serde_json::to_value(&attributes).expect("the attributes serialize");
    let object = encoded.as_object().expect("the attributes are an object");
    assert!(
        object.contains_key("userId"),
        "the hash attribute keeps its wire name"
    );
    assert!(object.contains_key("organizationId"));
    assert_eq!(
        object.len(),
        9,
        "every attribute is posted when every one is set"
    );
}
