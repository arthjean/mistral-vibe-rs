use serde_json::json;

use super::{Ordered, python_float};

#[test]
fn separators_and_indentation_follow_python() {
    let value = Ordered::of(&json!({"a": [1, {"b": null}], "c": [], "d": {}})).expect("value");
    assert_eq!(
        value.compact(),
        r#"{"a": [1, {"b": null}], "c": [], "d": {}}"#
    );
    assert_eq!(
        value.pretty(),
        "{\n  \"a\": [\n    1,\n    {\n      \"b\": null\n    }\n  ],\n  \"c\": [],\n  \"d\": {}\n}"
    );
}

#[test]
fn a_moved_key_lands_after_its_anchor() {
    #[derive(serde::Serialize)]
    struct Entry {
        r#type: &'static str,
        id: &'static str,
        related: Option<&'static str>,
        text: &'static str,
    }
    let mut value = Ordered::of(&Entry {
        r#type: "message",
        id: "1",
        related: None,
        text: "é\n",
    })
    .expect("value");
    value.move_after("type", "related");
    assert_eq!(
        value.compact(),
        r#"{"id": "1", "related": null, "type": "message", "text": "é\n"}"#
    );
}

#[test]
fn floats_are_spelled_as_python_repr_spells_them() {
    for (value, expected) in [
        (0.0, "0.0"),
        (3.712_556_001_119_083, "3.712556001119083"),
        (1.0, "1.0"),
        (0.0001, "0.0001"),
        (0.000_01, "1e-05"),
        (1e16, "1e+16"),
        (123_456.5, "123456.5"),
        (-2.5e-7, "-2.5e-07"),
        (1.5e15, "1500000000000000.0"),
    ] {
        assert_eq!(python_float(value), expected, "{value}");
    }
}
