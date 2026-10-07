use serde_json::json;

use super::{canonical_json, es6_number, normalize_nfc};

#[test]
fn objects_sort_by_utf16_code_units_and_strings_escape_minimally() {
    let value =
        json!({"b": 1, "a": "x\u{1}/\"é", "\u{e9}": null, "\u{1f600}": true, "\u{ffff}": false});
    let bytes = canonical_json(&value).expect("serializes");
    assert_eq!(
        String::from_utf8(bytes).expect("utf-8"),
        "{\"a\":\"x\\u0001/\\\"é\",\"b\":1,\"\u{e9}\":null,\"\u{1f600}\":true,\"\u{ffff}\":false}"
    );
}

#[test]
fn doubles_lay_out_as_ecmascript_writes_them() {
    for (value, expected) in [
        (1.0, "1"),
        (0.5, "0.5"),
        (-0.0, "0"),
        (1e21, "1e+21"),
        (1e20, "100000000000000000000"),
        (1e-7, "1e-7"),
        (0.000_001, "0.000001"),
        (123.456, "123.456"),
        (1.5e-10, "1.5e-10"),
    ] {
        assert_eq!(es6_number(value).expect("finite"), expected, "{value}");
    }
}

#[test]
fn integers_beyond_the_safe_range_are_refused() {
    assert!(canonical_json(&json!(9_007_199_254_740_992_u64)).is_err());
    assert!(canonical_json(&json!(9_007_199_254_740_991_u64)).is_ok());
}

#[test]
fn decomposed_strings_compose() {
    assert_eq!(normalize_nfc("e\u{301}"), "\u{e9}");
}
