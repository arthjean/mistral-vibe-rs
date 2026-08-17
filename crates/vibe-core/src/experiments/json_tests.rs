//! The two properties the variant strings rest on: key order survives a
//! decode, and the text matches what `json.dumps` writes.

use super::json::{JsonValue, OrderedMap};

fn decode(text: &str) -> JsonValue {
    serde_json::from_str(text).expect("the oracle document parses")
}

#[test]
fn an_object_keeps_the_order_its_keys_arrived_in() {
    let value = decode(r#"{"zebra": 1, "alpha": 2, "middle": 3}"#);
    let object = value.as_object().expect("the document is an object");
    assert_eq!(
        object.keys().collect::<Vec<_>>(),
        ["zebra", "alpha", "middle"]
    );
    assert_eq!(
        value.python_json(),
        r#"{"zebra": 1, "alpha": 2, "middle": 3}"#
    );
}

#[test]
fn a_duplicate_key_keeps_the_first_position_and_the_last_value() {
    let value = decode(r#"{"a": 1, "b": 2, "a": 3}"#);
    assert_eq!(value.python_json(), r#"{"a": 3, "b": 2}"#);
}

#[test]
fn a_nested_object_keeps_its_order_too() {
    let value = decode(
        r#"{"active_model": "alias", "model_config": {"name": "n", "provider": "p", "alias": "a"}}"#,
    );
    assert_eq!(
        value.python_json(),
        r#"{"active_model": "alias", "model_config": {"name": "n", "provider": "p", "alias": "a"}}"#
    );
}

#[test]
fn every_scalar_renders_the_way_python_renders_it() {
    assert_eq!(JsonValue::Null.python_json(), "null");
    assert_eq!(JsonValue::Bool(true).python_json(), "true");
    assert_eq!(JsonValue::Bool(false).python_json(), "false");
    assert_eq!(decode("7").python_json(), "7");
    assert_eq!(decode("0").python_json(), "0");
    assert_eq!(decode("-1").python_json(), "-1");
    assert_eq!(decode("1.5").python_json(), "1.5");
    assert_eq!(decode("1.0").python_json(), "1.0");
    assert_eq!(decode(r#""text""#).python_json(), r#""text""#);
    assert_eq!(decode("[]").python_json(), "[]");
    assert_eq!(decode("{}").python_json(), "{}");
}

#[test]
fn an_array_separates_its_items_with_a_space() {
    assert_eq!(
        decode(r#"["cli", "lean"]"#).python_json(),
        r#"["cli", "lean"]"#
    );
    assert_eq!(decode("[1, [2, 3]]").python_json(), "[1, [2, 3]]");
}

#[test]
fn a_string_is_escaped_the_way_json_dumps_escapes_it() {
    // The two mandatory escapes and the five short control escapes.
    assert_eq!(
        JsonValue::String("a\"b\\c\nd\te\rf".to_owned()).python_json(),
        r#""a\"b\\c\nd\te\rf""#
    );
    assert_eq!(
        JsonValue::String("\u{8}\u{c}".to_owned()).python_json(),
        r#""\b\f""#
    );
    // Every other control character, and every character above the printable
    // ASCII range, because `ensure_ascii` defaults to true.
    assert_eq!(
        JsonValue::String("\u{1}".to_owned()).python_json(),
        "\"\\u0001\""
    );
    assert_eq!(
        JsonValue::String("\u{7f}".to_owned()).python_json(),
        "\"\\u007f\""
    );
    assert_eq!(
        JsonValue::String("\u{e9}".to_owned()).python_json(),
        "\"\\u00e9\""
    );
    // Above the basic multilingual plane, as the surrogate pair Python writes.
    assert_eq!(
        JsonValue::String("\u{1f642}".to_owned()).python_json(),
        "\"\\ud83d\\ude42\""
    );
    // A forward slash is not escaped, which is where several JSON writers
    // differ from Python.
    assert_eq!(
        JsonValue::String("a/b".to_owned()).python_json(),
        r#""a/b""#
    );
}

#[test]
fn a_value_round_trips_through_serde_keeping_its_order() {
    let value = decode(r#"{"z": [1, {"b": null, "a": true}], "y": "x"}"#);
    let encoded = serde_json::to_string(&value).expect("the value serializes");
    let decoded: JsonValue = serde_json::from_str(&encoded).expect("the value parses back");
    assert_eq!(decoded, value);
    assert_eq!(decoded.python_json(), value.python_json());
}

#[test]
fn an_ordered_map_replaces_in_place_and_retains_in_order() {
    let mut map = OrderedMap::new();
    map.insert("first".to_owned(), 1);
    map.insert("second".to_owned(), 2);
    map.insert("third".to_owned(), 3);
    map.insert("first".to_owned(), 10);
    assert_eq!(map.get("first"), Some(&10));
    assert_eq!(map.len(), 3);
    assert!(!map.is_empty());
    assert_eq!(map.keys().collect::<Vec<_>>(), ["first", "second", "third"]);
    map.retain(|key, _| key != "second");
    assert_eq!(map.keys().collect::<Vec<_>>(), ["first", "third"]);
    assert_eq!(map.get("second"), None);
    assert!(OrderedMap::<i32>::new().is_empty());
}

#[test]
fn a_non_object_is_not_read_as_one() {
    assert_eq!(decode("7").as_object(), None);
    assert_eq!(decode("7").as_str(), None);
    assert_eq!(decode(r#""text""#).as_str(), Some("text"));
    assert!(JsonValue::Null.is_null());
    assert!(!decode("0").is_null());
}
