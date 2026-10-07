//! The foreign adapters' own helpers. What the adapters resolve is measured
//! against the reference in `plugins_parity_tests`.

use std::path::Path;

use serde_json::{Value, json};

#[test]
fn jsonc_comments_and_trailing_commas_are_removed_outside_strings() {
    let text = "{\"a\": \"//not\", /* gone */ \"b\": [1, 2,], // tail\n}";
    let cleaned = super::remove_trailing_commas(&super::remove_json_comments(text).unwrap());
    let value: Value = serde_json::from_str(&cleaned).unwrap();
    assert_eq!(value, json!({"a": "//not", "b": [1, 2]}));
    assert!(super::remove_json_comments("{/* open").is_err());
}

#[test]
fn a_command_source_name_drops_only_the_last_suffix() {
    let base = Path::new("/p/commands");
    assert_eq!(
        super::command_source_name(Path::new("/p/commands/a/b.md"), base),
        "a/b"
    );
    assert_eq!(
        super::command_source_name(Path::new("/p/commands/x.y.md"), base),
        "x.y"
    );
    assert_eq!(
        super::command_source_name(Path::new("/p/commands/.md"), base),
        ".md"
    );
}
