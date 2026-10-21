//! Python's `==` over decoded JSON, which the configuration variants drop a
//! default-valued variant by.

use serde_json::{Value, json};

use super::json::python_eq;

fn decode(text: &str) -> Value {
    serde_json::from_str(text).expect("the oracle document parses")
}

#[test]
fn a_boolean_equals_the_integer_and_the_float_it_stands_for() {
    assert!(python_eq(&json!(false), &json!(0)));
    assert!(python_eq(&json!(true), &json!(1.0)));
    assert!(python_eq(&json!(1), &decode("1.0")));
    assert!(!python_eq(&json!(true), &json!(2)));
    assert!(!python_eq(&decode("1.5"), &json!(1)));
}

#[test]
fn an_object_compares_regardless_of_key_order() {
    assert!(python_eq(
        &decode(r#"{"a": 1, "b": [true, null]}"#),
        &decode(r#"{"b": [1, null], "a": 1.0}"#)
    ));
    assert!(!python_eq(
        &decode(r#"{"a": 1}"#),
        &decode(r#"{"a": 1, "b": 2}"#)
    ));
}

#[test]
fn values_of_different_kinds_never_compare_equal() {
    assert!(python_eq(&Value::Null, &Value::Null));
    assert!(!python_eq(&json!("0"), &json!(0)));
    assert!(!python_eq(&json!([]), &json!({})));
    assert!(!python_eq(&Value::Null, &json!(false)));
    assert!(!python_eq(&json!(["a", "b"]), &json!(["b", "a"])));
}
