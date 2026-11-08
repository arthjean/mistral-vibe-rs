//! Replays the reference's own validation outcomes against this module.
//!
//! `tests/wire-validation/cases.json` holds, for every request parameter model
//! the reference declares, inputs and what pydantic answered: the location and
//! type of each issue in order, or the value it produced.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::Value;
use vibe_core::parity::figures::Figures;
use vibe_core::parity::replay::{Class, Entry as _, PatternDivergence, assert_ledger, entries};
use vibe_protocol::PathSegment;

use super::{SCHEMA, custom, ordered_params, validate_method, validate_model_named};

const CASES: &str = include_str!("../../tests/wire-validation/cases.json");

#[derive(Deserialize)]
struct Corpus {
    cases: Vec<Case>,
}

#[derive(Deserialize)]
struct Case {
    model: String,
    input: Value,
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
    vibe_core::parity::manifest::assert_corpus(
        "crates/vibe-app-server/src/wire_validation/schema.json",
    );
    for function in &SCHEMA.functions {
        assert!(
            custom::KNOWN.contains(&function.as_str()),
            "the reference validator {function} has no counterpart"
        );
    }
}

/// The corpus as the manifest lists it.
const LISTED: &str = "crates/vibe-app-server/tests/wire-validation/cases.json";

/// What this port still answers differently, by model and by the field the
/// reference locates its first issue at. Row 17 is what the corpus measures:
/// these are gaps the v3.1.0 re-pin revealed.
const LEDGER: &[PatternDivergence] = &[
    PatternDivergence {
        scenario: "MCPAddParams",
        pointer: "/errors/transport",
        count: 15,
        row: "17",
        class: Class::Gap,
        reason: "v3.0.0 made `transport` a union of literals, which pydantic refuses once per \
             choice; the reduced schema records one literal over their joint values \
             (scripts/parity/app_server_params.py), so this port refuses once",
    },
    PatternDivergence {
        scenario: "SessionArchiveParams",
        pointer: "/value/root",
        count: 1,
        row: "17",
        class: Class::Gap,
        reason: DEFAULTS_DROPPED,
    },
    PatternDivergence {
        scenario: "SessionListParams",
        pointer: "/value/root",
        count: 2,
        row: "17",
        class: Class::Gap,
        reason: DEFAULTS_DROPPED,
    },
];

/// Why the v3.1.0 dump of some parameters is shorter than this port's.
const DEFAULTS_DROPPED: &str = "v3.0.0 leaves out of what it validates a field the client sent at its \
                                default (a false `requireIdle`, an empty `include`, a null \
                                `sessionIds`), which the reduced schema does not record, so this \
                                port keeps it";

/// Where a differing case is ledgered: its model, and the field of the
/// reference's first issue (`/value` when the reference accepted it).
fn ledger_pointer(case: &Case) -> String {
    match case.errors.as_deref().and_then(<[_]>::first) {
        Some((loc, _)) => match loc.first() {
            Some(Value::String(field)) => format!("/errors/{field}"),
            Some(other) => format!("/errors/{other}"),
            None => "/errors/root".to_owned(),
        },
        None => "/value/root".to_owned(),
    }
}

#[test]
fn validation_answers_as_the_reference_does() {
    vibe_core::parity::manifest::assert_corpus(LISTED);
    let corpus: Corpus = serde_json::from_str(CASES).unwrap();
    let mut unexplained = Vec::new();
    let mut covered = vec![0_usize; LEDGER.len()];
    let mut conformant = 0;
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
        if equal {
            conformant += 1;
            continue;
        }
        let pointer = ledger_pointer(case);
        match LEDGER
            .iter()
            .position(|entry| entry.covers(&case.model, &pointer))
        {
            Some(index) => covered[index] += 1,
            None => unexplained.push(format!(
                "{} {} ({pointer})\n  expected {expected}\n  observed {observed}",
                case.model, case.input
            )),
        }
    }
    assert!(
        unexplained.is_empty(),
        "{} of {} cases differ where no ledger entry says they may:\n{}",
        unexplained.len(),
        corpus.cases.len(),
        unexplained
            .iter()
            .take(40)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
    let miscounted: Vec<String> = LEDGER
        .iter()
        .zip(&covered)
        .filter(|(entry, found)| entry.count != **found)
        .map(|(entry, found)| {
            format!(
                "{} declares {} differences and covered {found}",
                entry.label(),
                entry.count
            )
        })
        .collect();
    assert!(miscounted.is_empty(), "{}", miscounted.join("\n"));
    let recorded: BTreeMap<String, &Value> = corpus
        .cases
        .iter()
        .map(|case| (case.model.clone(), &case.input))
        .collect();
    assert_ledger(LISTED, &recorded, entries(LEDGER));
    vibe_core::parity::figures::assert_figures(
        "wire validation parity:",
        LISTED,
        Figures {
            conformant,
            accepted: 0,
            total: corpus.cases.len(),
            accepted_differences: 0,
            gap_differences: covered.iter().sum(),
            prose_differences: 0,
        },
        serde_json::json!({}),
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
        let input = serde_json::json!({
            "sessionId": "s",
            "input": [{"type": tag}],
        });
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

/// The parameters read back from the raw frame keep the order the client
/// wrote, a duplicated key in its first position with its last value, so
/// extra keys are reported in that order rather than sorted.
#[test]
fn raw_frame_parameters_keep_the_written_key_order() {
    let frame = br#"{"jsonrpc":"2.0","id":1,"method":"agents/list","params":{"zeta":1,"sessionId":"s","alpha":2,"zeta":3}}"#;
    let params = ordered_params(frame).expect("the frame carries parameters");
    let object = params.as_object().expect("the parameters are an object");
    assert_eq!(
        object.keys().map(String::as_str).collect::<Vec<_>>(),
        ["zeta", "sessionId", "alpha"]
    );
    assert_eq!(object["zeta"], 3);
    let issues = validate_method("agents/list", &params).expect_err("two keys are extra");
    assert_eq!(
        issues
            .iter()
            .map(|issue| (location(&issue.path), issue.kind))
            .collect::<Vec<_>>(),
        [
            (vec![Value::from("zeta")], "extra_forbidden"),
            (vec![Value::from("alpha")], "extra_forbidden"),
        ]
    );
}
