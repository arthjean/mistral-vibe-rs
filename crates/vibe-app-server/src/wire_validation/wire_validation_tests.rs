//! Replays the reference's own validation outcomes against this module.
//!
//! `tests/wire-validation/cases.json` holds, for every request parameter model
//! the reference declares, inputs and what pydantic answered: the location and
//! type of each issue in order, or the value it produced.

use serde::Deserialize;
use serde_json::Value;
use vibe_protocol::PathSegment;

use super::{Json, SCHEMA, custom, validate_model_named};

const CASES: &str = include_str!("../../tests/wire-validation/cases.json");

#[derive(Deserialize)]
struct Corpus {
    cases: Vec<Case>,
}

#[derive(Deserialize)]
struct Case {
    model: String,
    input: Json,
    #[serde(default)]
    errors: Option<Vec<(Vec<Value>, String)>>,
    #[serde(default)]
    value: Option<Value>,
}

fn location(path: &[PathSegment]) -> Vec<Value> {
    path.iter()
        .map(|segment| match segment {
            PathSegment::Field(name) => Value::String(name.clone()),
            PathSegment::Index(index) => Value::from(*index),
        })
        .collect()
}

/// Equality as the JSON dump reads it: `5` and `5.0` are the same number.
fn same_value(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Number(left), Value::Number(right)) => left.as_f64() == right.as_f64(),
        (Value::Array(left), Value::Array(right)) => {
            left.len() == right.len() && left.iter().zip(right).all(|(l, r)| same_value(l, r))
        }
        (Value::Object(left), Value::Object(right)) => {
            left.len() == right.len()
                && left.iter().all(|(key, value)| {
                    right.get(key).is_some_and(|other| same_value(value, other))
                })
        }
        _ => left == right,
    }
}

#[test]
fn the_compiled_schema_parses() {
    assert!(!SCHEMA.defs.is_empty());
    assert!(SCHEMA.methods.contains_key("session/start"));
}

#[test]
fn every_validator_the_schema_names_is_implemented() {
    for function in &SCHEMA.functions {
        assert!(
            custom::KNOWN.contains(&function.as_str()),
            "the reference validator {function} has no counterpart"
        );
    }
}

#[test]
fn validation_answers_as_the_reference_does() {
    let corpus: Corpus = serde_json::from_str(CASES).unwrap();
    let mut failures = Vec::new();
    for case in &corpus.cases {
        let outcome = validate_model_named(&case.model, &case.input);
        let observed = match &outcome {
            Ok(value) => format!("value {value}"),
            Err(issues) => format!(
                "errors {:?}",
                issues
                    .iter()
                    .map(|issue| (location(&issue.path), issue.kind))
                    .collect::<Vec<_>>()
            ),
        };
        let expected = match (&case.errors, &case.value) {
            (Some(errors), _) => format!(
                "errors {:?}",
                errors
                    .iter()
                    .map(|(loc, kind)| (loc.clone(), kind.as_str()))
                    .collect::<Vec<_>>()
            ),
            (None, Some(value)) => format!("value {value}"),
            (None, None) => "nothing".to_owned(),
        };
        let equal = match (&outcome, &case.value) {
            (Ok(value), Some(expected)) if case.errors.is_none() => same_value(value, expected),
            _ => observed == expected,
        };
        if !equal {
            failures.push(format!(
                "{} {}\n  expected {expected}\n  observed {observed}",
                case.model,
                case.input.clone().into_value()
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} cases differ:\n{}",
        failures.len(),
        corpus.cases.len(),
        failures
            .iter()
            .take(40)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// Pydantic quotes a tag that matches no choice with Python's `str`, so a
/// container or an exponent float reads the way Python prints it. Measured on
/// the pinned reference with `ContextInjectParams`.
#[test]
fn an_unmatched_tag_is_quoted_with_python_str() {
    for (tag, quoted) in [
        (serde_json::json!([1, 2]), "[1, 2]"),
        (serde_json::json!(1e16), "1e+16"),
        (serde_json::json!({"a": 1}), "{'a': 1}"),
    ] {
        let input = Json::from_value(&serde_json::json!({
            "sessionId": "s",
            "input": [{"type": tag}],
        }));
        let issues = validate_model_named("ContextInjectParams", &input)
            .expect_err("the tag matches no content block");
        assert_eq!(issues[0].kind, "union_tag_invalid");
        assert_eq!(
            issues[0].message,
            format!(
                "Input tag '{quoted}' found using 'type' does not match any of the expected \
                 tags: 'text', 'image', 'resource'"
            )
        );
    }
}
