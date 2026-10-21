//! The OpenCode adapter's own helpers. What the adapter resolves is measured
//! against the reference in `plugins_parity_tests`.

use serde_json::{Value, json};

#[test]
fn jsonc_comments_and_trailing_commas_are_removed_outside_strings() {
    let text = "{\"a\": \"//not\", /* gone */ \"b\": [1, 2,], // tail\n}";
    let cleaned = super::remove_trailing_commas(&super::remove_json_comments(text).unwrap());
    let value: Value = serde_json::from_str(&cleaned).unwrap();
    assert_eq!(value, json!({"a": "//not", "b": [1, 2]}));
    assert!(super::remove_json_comments("{/* open").is_err());
}
