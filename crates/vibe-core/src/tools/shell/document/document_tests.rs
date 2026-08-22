//! The rendering rules the corpus cannot reach.
//!
//! The replayed corpus proves the ordinary shapes end to end. What it never
//! carries is a value whose Python `repr` needs an escape, so the quoting rules
//! are pinned here instead.

use serde_json::{Value, json};

use super::Document;

fn rendered(value: Value) -> String {
    Document::new().field("field", value).model_text()
}

#[test]
fn a_top_level_string_is_rendered_as_itself() {
    assert_eq!(rendered(json!("a\nb")), "field: a\nb");
}

#[test]
fn scalars_are_rendered_with_python_spelling() {
    assert_eq!(rendered(Value::Null), "field: None");
    assert_eq!(rendered(json!(true)), "field: True");
    assert_eq!(rendered(json!(false)), "field: False");
    assert_eq!(rendered(json!(7)), "field: 7");
}

#[test]
fn a_nested_string_is_quoted_and_escaped() {
    let nested = Document::new().field("inner", "a\nb\t\\c\x07");
    let text = Document::new().nested("field", Some(nested)).model_text();
    assert_eq!(text, "field: {'inner': 'a\\nb\\t\\\\c\\x07'}");
}

#[test]
fn a_nested_string_carrying_a_quote_switches_delimiter() {
    let apostrophe = Document::new().field("inner", "it's");
    assert_eq!(
        Document::new()
            .nested("field", Some(apostrophe))
            .model_text(),
        "field: {'inner': \"it's\"}"
    );
    let both = Document::new().field("inner", "it's \"quoted\"");
    assert_eq!(
        Document::new().nested("field", Some(both)).model_text(),
        "field: {'inner': 'it\\'s \"quoted\"'}"
    );
}

#[test]
fn a_document_keeps_its_field_order_in_the_text_and_answers_json_by_name() {
    let document = Document::new()
        .field("zulu", 1)
        .field("alpha", 2)
        .nested_list("sessions", vec![Document::new().field("id", "s0")]);
    assert_eq!(
        document.model_text(),
        "zulu: 1\nalpha: 2\nsessions: [{'id': 's0'}]"
    );
    assert_eq!(document.get("alpha"), Some(&json!(2)));
    assert_eq!(
        document.typed(),
        json!({"zulu": 1, "alpha": 2, "sessions": [{"id": "s0"}]})
    );
}
