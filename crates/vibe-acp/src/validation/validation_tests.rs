use serde_json::{Value, json};

use super::validate_request;
use crate::protocol::AcpError;

fn errors(method: &str, params: Value) -> Vec<Value> {
    let outcome = validate_request(method, &params);
    let Err(AcpError::Validation(errors)) = outcome else {
        unreachable!("expected a validation failure, got {outcome:?}");
    };
    errors
}

#[test]
fn a_missing_field_is_located_and_carries_the_whole_input() {
    let errors = errors("session/new", json!({"cwd": "/w"}));
    assert_eq!(
        errors,
        vec![json!({
            "type": "missing",
            "loc": ["mcpServers"],
            "msg": "Field required",
            "input": {"cwd": "/w"},
            "url": "https://errors.pydantic.dev/2.13/v/missing",
        })]
    );
}

#[test]
fn a_union_reports_every_member_under_its_name() {
    let errors = errors(
        "session/prompt",
        json!({"sessionId": "s", "prompt": [{"type": "text"}]}),
    );
    let locations = errors
        .iter()
        .map(|error| error["loc"].clone())
        .collect::<Vec<_>>();
    assert_eq!(
        locations[0],
        json!(["prompt", 0, "TextContentBlock", "text"])
    );
    assert_eq!(
        locations[1],
        json!(["prompt", 0, "ImageContentBlock", "data"])
    );
    assert!(
        errors.iter().any(|error| error["type"] == "literal_error"
            && error["ctx"] == json!({"expected": "'image'"}))
    );
}

#[test]
fn lax_conversions_accept_what_the_validator_accepts() {
    assert!(
        validate_request(
            "initialize",
            &json!({"protocolVersion": "x", "clientCapabilities": {"fs": {"readTextFile": "yes"}}}),
        )
        .is_ok()
    );
    let errors = errors(
        "initialize",
        json!({"protocolVersion": 1, "clientCapabilities": {"terminal": "maybe"}}),
    );
    assert_eq!(errors[0]["type"], "bool_parsing");
}

#[test]
fn a_non_object_payload_is_a_model_type_error() {
    let errors = errors("session/new", Value::Null);
    assert_eq!(errors[0]["type"], "model_type");
    assert_eq!(errors[0]["loc"], json!([]));
}
