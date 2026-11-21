//! The ratchet of a measured debt, and the shape its file has to keep.

use std::collections::BTreeMap;

use serde_json::json;

use super::{
    CHECKS_PINS, Debt, Declared, Level, Pin, declared_group, document_problems, fingerprint,
    generalize, groups, path_of,
};

const LISTED: &str = "crates/sample/tests/sample-parity/corpus.json";
const MEASURED: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn debt(groups: &[(&str, &str, usize)]) -> Debt {
    Debt::of(
        LISTED,
        "sample parity:",
        groups
            .iter()
            .map(|(scenario, pointer, count)| {
                (((*scenario).to_owned(), (*pointer).to_owned()), *count)
            })
            .collect(),
    )
}

#[test]
fn a_debt_file_sits_under_the_debt_directory_named_after_its_corpus() {
    assert_eq!(
        path_of(LISTED),
        "scripts/parity/debt/sample--tests--sample-parity--corpus.json"
    );
}

#[test]
fn a_pointer_is_grouped_with_its_array_indices_generalized() {
    assert_eq!(
        generalize("/3/stream/12/message/params/v2"),
        "/*/stream/*/message/params/v2"
    );
}

#[test]
fn a_debt_covers_its_own_groups_and_nothing_else() {
    let mut debt = debt(&[("flows/a", "/*/result/budget", 2)]);
    assert!(debt.absorb("flows/a", "/3/result/budget"));
    assert!(debt.absorb("flows/a", "/7/result/budget"));
    assert!(
        !debt.absorb("flows/b", "/3/result/budget"),
        "a group holds one exact scenario, so a difference cannot move to another unnoticed"
    );
    assert!(!debt.absorb("flows/a", "/3/result/limit"));
    assert_eq!(debt.covered(), 2);
    assert!(debt.problems().is_empty(), "{:?}", debt.problems());
}

#[test]
fn a_group_that_grows_is_a_regression_and_one_that_shrinks_is_progress_to_record() {
    let mut grown = debt(&[("flows/a", "/*/result", 1)]);
    assert!(grown.absorb("flows/a", "/1/result"));
    assert!(grown.absorb("flows/a", "/2/result"));
    let problems = grown.problems();
    assert!(
        problems.len() == 1 && problems[0].contains("the port regressed inside its debt"),
        "{problems:?}"
    );

    let shrunk = debt(&[("flows/a", "/*/result", 3)]);
    let problems = shrunk.problems();
    assert!(
        problems.len() == 1
            && problems[0].contains("differs 0 times where the debt measured 3")
            && problems[0].contains(&format!("repin.py debt {LISTED}")),
        "{problems:?}"
    );
}

#[test]
fn a_debt_file_names_its_corpus_its_measurement_and_positive_generalized_groups() {
    let sound = json!({
        "schemaVersion": 2,
        "corpus": LISTED,
        "measuredAgainst": MEASURED,
        "tallies": {"sample parity:": {"flows/a": {
            "/*/result": 2,
            "/*/limit": {"count": 1, "level": "values", "fingerprint": "0123456789abcdef"},
        }}},
    });
    assert!(document_problems(&sound, LISTED, MEASURED).is_empty());
    assert_eq!(
        groups(&sound, "sample parity:"),
        BTreeMap::from([
            (
                ("flows/a".to_owned(), "/*/limit".to_owned()),
                Declared {
                    count: 1,
                    pin: Some(Pin {
                        level: Level::Values,
                        fingerprint: "0123456789abcdef".to_owned()
                    })
                }
            ),
            (
                ("flows/a".to_owned(), "/*/result".to_owned()),
                Declared {
                    count: 2,
                    pin: None
                }
            ),
        ])
    );
    assert!(groups(&sound, "another parity:").is_empty());

    let mut moved = sound.clone();
    moved["measuredAgainst"] = json!("f".repeat(64));
    moved["tallies"]["sample parity:"]["flows/a"] = json!({"/3/result": 0});
    let problems = document_problems(&moved, LISTED, MEASURED);
    for expected in [
        "it was measured against",
        "is not a pointer with its indices generalized",
        "declares no positive count",
    ] {
        assert!(
            problems.iter().any(|problem| problem.contains(expected)),
            "{expected}: {problems:?}"
        );
    }

    for unsound in [
        json!({"count": 1, "level": "loose", "fingerprint": "0123456789abcdef"}),
        json!({"count": 1, "level": "exact", "fingerprint": "0123"}),
        json!({"count": 0, "level": "exact", "fingerprint": "0123456789abcdef"}),
        json!({"count": 1, "level": "exact"}),
    ] {
        assert_eq!(declared_group(&unsound), None, "{unsound}");
    }

    let empty = json!({
        "schemaVersion": 2,
        "corpus": LISTED,
        "measuredAgainst": MEASURED,
        "tallies": {},
    });
    assert!(
        document_problems(&empty, LISTED, MEASURED)
            .iter()
            .any(|problem| problem.contains("a corpus without debt keeps no file"))
    );
}

fn pinned(level: Level, answers: &[(&str, serde_json::Value)]) -> Debt {
    let answers: Vec<(String, Option<serde_json::Value>)> = answers
        .iter()
        .map(|(pointer, value)| ((*pointer).to_owned(), Some(value.clone())))
        .collect();
    Debt::pinned(
        LISTED,
        "sample parity:",
        BTreeMap::from([(
            ("flows/a".to_owned(), "/*/result".to_owned()),
            Declared {
                count: answers.len(),
                pin: Some(Pin {
                    level,
                    fingerprint: fingerprint(level, &answers),
                }),
            },
        )]),
    )
}

#[test]
fn a_pinned_group_sees_a_change_inside_its_gap_at_its_level() {
    let measured = [("/1/result", json!("wrong")), ("/2/result", json!(7))];
    // The same answers, found in another order, keep every pin.
    for level in Level::ALL {
        let mut debt = pinned(level, &measured);
        assert!(debt.absorb_value("flows/a", "/2/result", Some(&json!(7))));
        assert!(debt.absorb_value("flows/a", "/1/result", Some(&json!("wrong"))));
        assert!(
            debt.problems().is_empty(),
            "{level:?}: {:?}",
            debt.problems()
        );
    }
    // One more wrong answer of the same type and length holds the count and
    // the shape, and moves the values.
    let changed = |level| {
        let mut debt = pinned(level, &measured);
        debt.absorb_value("flows/a", "/1/result", Some(&json!("wronk")));
        debt.absorb_value("flows/a", "/2/result", Some(&json!(7)));
        debt.problems()
    };
    assert!(changed(Level::Shape).is_empty());
    for level in [Level::Exact, Level::Values] {
        let problems = changed(level);
        assert_eq!(
            problems.len(),
            usize::from(CHECKS_PINS),
            "{level:?}: {problems:?}"
        );
        if CHECKS_PINS {
            assert!(
                problems[0].contains("a change inside a known gap"),
                "{problems:?}"
            );
        }
    }
    // An answer that moves to another index keeps the values and moves the
    // exact pin.
    let mut moved = pinned(Level::Exact, &measured);
    moved.absorb_value("flows/a", "/3/result", Some(&json!("wrong")));
    moved.absorb_value("flows/a", "/2/result", Some(&json!(7)));
    assert_eq!(moved.problems().len(), usize::from(CHECKS_PINS));
    // A replay that absorbs without the answers cannot satisfy a pin.
    let mut blind = pinned(Level::Shape, &measured);
    blind.absorb("flows/a", "/1/result");
    blind.absorb("flows/a", "/2/result");
    assert_eq!(blind.problems().len(), usize::from(CHECKS_PINS));
}

#[test]
fn a_fingerprint_reads_values_whatever_their_key_order() {
    let answers = |value: serde_json::Value| vec![("/0/result".to_owned(), Some(value))];
    let left = serde_json::from_str::<serde_json::Value>(r#"{"a": 1, "b": [2, {"d": 4, "c": 3}]}"#)
        .expect("json");
    let right =
        serde_json::from_str::<serde_json::Value>(r#"{"b": [2, {"c": 3, "d": 4}], "a": 1}"#)
            .expect("json");
    for level in Level::ALL {
        assert_eq!(
            fingerprint(level, &answers(left.clone())),
            fingerprint(level, &answers(right.clone()))
        );
    }
    assert_ne!(
        fingerprint(Level::Shape, &answers(json!("abc"))),
        fingerprint(Level::Shape, &answers(json!(3))),
        "a type the reference never sends is a change of shape"
    );
    assert_ne!(
        fingerprint(Level::Shape, &answers(json!({"prose": 3, "sha256": "a"}))),
        fingerprint(Level::Shape, &answers(json!({"prose": 4, "sha256": "a"}))),
        "a prose digest's shape is its length"
    );
}
