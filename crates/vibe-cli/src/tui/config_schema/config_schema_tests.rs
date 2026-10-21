use serde_json::json;

use super::*;

#[test]
fn secret_detection_covers_schema_and_conventional_names() {
    assert!(field_is_secret(["api_key"], &json!({"type": "string"})));
    assert!(field_is_secret(
        ["credential"],
        &json!({"type": "string", "writeOnly": true})
    ));
    assert!(field_is_secret(
        "providers.mistral.Token".split('.'),
        &json!({"type": "string"})
    ));
    assert!(!field_is_secret(
        ["active_model"],
        &json!({"type": "string"})
    ));
}

#[test]
fn declared_types_read_one_word_or_a_list() {
    assert_eq!(declared_types(&json!({"type": "integer"})), ["integer"]);
    assert_eq!(
        declared_types(&json!({"type": ["string", "null"]})),
        ["string", "null"]
    );
    assert!(declared_types(&json!({"enum": ["a"]})).is_empty());
}
