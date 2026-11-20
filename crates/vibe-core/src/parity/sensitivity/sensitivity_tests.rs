use std::collections::BTreeMap;

use serde_json::{Value, json};

use super::{Blind, Matrix, Mutation, Operator, PROBE_KEY, is_digest, mutate, rank, sites};
use crate::parity::debt::Debt;
use crate::parity::replay::{Class, Entry, PatternDivergence, Tally};

fn document() -> Value {
    json!({
        "name": "alpha",
        "count": 3,
        "ok": true,
        "missing": null,
        "items": ["a", "b"],
        "text": {"prose": 12, "sha256": "ab12"},
        "nested": {"deep": [1, 1]},
    })
}

#[test]
fn every_operator_finds_its_sites_and_changes_the_document_there() {
    let value = document();
    for operator in Operator::ALL {
        let found = sites(&value, operator);
        assert!(!found.is_empty(), "{operator} found no site");
        for pointer in &found {
            let mutated = mutate(&value, operator, pointer)
                .unwrap_or_else(|| panic!("{operator} did not apply at {pointer}"));
            assert_ne!(mutated, value, "{operator} left {pointer} unchanged");
        }
    }
}

#[test]
fn a_digest_is_a_string_to_the_operators() {
    let value = document();
    let deletable = sites(&value, Operator::DeleteKey);
    assert!(deletable.contains(&"/text".to_owned()));
    assert!(
        !deletable
            .iter()
            .any(|pointer| pointer.starts_with("/text/"))
    );
    assert!(!sites(&value, Operator::AddKey).contains(&"/text".to_owned()));
    let longer = mutate(&value, Operator::ChangeLength, "/text").expect("applies");
    assert_eq!(longer["text"], json!({"prose": 13, "sha256": "ab12"}));
    let changed = mutate(&value, Operator::ChangeValue, "/text").expect("applies");
    assert_eq!(changed["text"]["prose"], 12);
    assert_ne!(changed["text"]["sha256"], "ab12");
    assert!(is_digest(&changed["text"]));
    // A pair that names the length any other way is no digest: the one shape
    // a capture records is `{prose, sha256}`.
    assert!(!is_digest(&json!({"length": 4, "sha256": "ff"})));
    assert!(!is_digest(&json!({"prose": 4, "digest": "ff"})));
    assert_eq!(
        mutate(
            &json!({"prose": 4, "sha256": "ff"}),
            Operator::ChangeLength,
            ""
        )
        .expect("applies"),
        json!({"prose": 5, "sha256": "ff"})
    );
}

#[test]
fn each_operator_does_what_it_names() {
    let value = document();
    let deleted = mutate(&value, Operator::DeleteKey, "/name").expect("applies");
    assert!(deleted.get("name").is_none());
    let added = mutate(&value, Operator::AddKey, "/nested").expect("applies");
    assert_eq!(added["nested"][PROBE_KEY], true);
    let typed = mutate(&value, Operator::ChangeType, "/count").expect("applies");
    assert_eq!(typed["count"], "3");
    let typed = mutate(&value, Operator::ChangeType, "/missing").expect("applies");
    assert_eq!(typed["missing"], false);
    let valued = mutate(&value, Operator::ChangeValue, "/name").expect("applies");
    assert_eq!(valued["name"].as_str().map(str::len), Some(5));
    let longer = mutate(&value, Operator::ChangeLength, "/name").expect("applies");
    assert_eq!(longer["name"], "alphax");
    let reordered = mutate(&value, Operator::Reorder, "/items").expect("applies");
    assert_eq!(reordered["items"], json!(["b", "a"]));
    let dropped = mutate(&value, Operator::DropElement, "/items/1").expect("applies");
    assert_eq!(dropped["items"], json!(["a"]));
    assert!(
        !sites(&value, Operator::Reorder).contains(&"/nested/deep".to_owned()),
        "a list of equal elements has no order to change"
    );
}

#[test]
fn the_ranking_is_stable_across_runs_and_hosts() {
    // A change to the hash moves every sampled site, so it is pinned.
    assert_eq!(rank(0, &[]), rank(0, &[]));
    assert_ne!(rank(0, &["a", "b"]), rank(0, &["ab"]));
    assert_ne!(rank(1, &["a"]), rank(2, &["a"]));
    assert_eq!(rank(super::SEED, &["corpus", "op"]), 0xad37_c357_ab4b_f1bf);
}

fn mutation(detected: bool, masked: bool) -> Mutation {
    Mutation {
        scenario: "s".to_owned(),
        pointer: "/a".to_owned(),
        detected,
        masked_by: masked.then(|| "s /a".to_owned()),
    }
}

#[test]
fn below_the_threshold_only_an_unseen_mutation_needs_a_blind_entry() {
    let mut matrix = Matrix::default();
    matrix.operators.insert(
        Operator::ChangeValue,
        vec![
            mutation(false, true),
            mutation(false, false),
            mutation(true, false),
        ],
    );
    matrix.operators.insert(
        Operator::DeleteKey,
        vec![mutation(false, true), mutation(true, false)],
    );
    let problems = matrix.problems(&[]);
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(
        problems[0].starts_with("change-value: 1 of 3 mutations unseen"),
        "{problems:?}"
    );

    let unseen = Blind {
        operator: Operator::ChangeValue,
        reason: "the entry admits any value",
    };
    assert!(matrix.problems(&[unseen]).is_empty());

    let stale = Blind {
        operator: Operator::DeleteKey,
        reason: "a masked mutation needs no entry",
    };
    let problems = matrix.problems(&[unseen, stale]);
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(
        problems[0].contains("blind entry for delete-key is stale"),
        "{problems:?}"
    );

    let silent = Blind {
        operator: Operator::ChangeValue,
        reason: " ",
    };
    assert!(
        matrix
            .problems(&[silent])
            .iter()
            .any(|problem| problem.contains("states no reason"))
    );
}

#[test]
fn an_oracle_at_or_above_the_threshold_needs_no_blind_entry() {
    let mut matrix = Matrix::default();
    let mut mutations = vec![mutation(true, false); 19];
    mutations.push(mutation(false, false));
    matrix.operators.insert(Operator::ChangeValue, mutations);
    assert!((matrix.rate() - 0.95).abs() < f64::EPSILON);
    assert!(matrix.problems(&[]).is_empty());
}

const ANY_NAME: PatternDivergence = PatternDivergence {
    scenario: "s",
    pointer: "/name",
    count: 1,
    row: "1",
    class: Class::Gap,
    reason: "the port names it its own way",
};

#[test]
fn a_mutation_an_entry_already_admits_is_undetected_and_the_rest_are_detected() {
    let reference = json!({"name": "reference", "size": 2, "tags": ["x", "y"]});
    let port = json!({"name": "port", "size": 2, "tags": ["x", "y"]});
    let ledger: [&dyn Entry; 1] = [&ANY_NAME];
    let mut tally = Tally::detached("crates/x/tests/corpus.json", ledger);
    tally.record("s", &reference, &port, vec!["/name".to_owned()]);
    let debt = Debt::of("crates/x/tests/corpus.json", "test:", BTreeMap::new());
    let matrix = tally.sensitivity_matrix(&debt, None);

    let outcome = |operator: Operator, pointer: &str| {
        matrix.operators[&operator]
            .iter()
            .find(|mutation| mutation.pointer == pointer)
            .map(|mutation| mutation.detected)
    };
    assert_eq!(outcome(Operator::ChangeValue, "/name"), Some(false));
    assert_eq!(outcome(Operator::ChangeLength, "/name"), Some(false));
    assert_eq!(outcome(Operator::ChangeType, "/name"), Some(false));
    // Deleting the ledgered key still leaves one difference at its pointer.
    assert_eq!(outcome(Operator::DeleteKey, "/name"), Some(false));
    assert_eq!(outcome(Operator::DeleteKey, "/size"), Some(true));
    assert_eq!(outcome(Operator::ChangeValue, "/size"), Some(true));
    assert_eq!(outcome(Operator::Reorder, "/tags"), Some(true));
    assert_eq!(outcome(Operator::DropElement, "/tags/0"), Some(true));
    assert_eq!(outcome(Operator::AddKey, ""), Some(true));
    let masked = |operator: Operator, pointer: &str| {
        matrix.operators[&operator]
            .iter()
            .find(|mutation| mutation.pointer == pointer)
            .and_then(|mutation| mutation.masked_by.clone())
    };
    assert_eq!(
        masked(Operator::ChangeValue, "/name").as_deref(),
        Some("s /name")
    );
    assert_eq!(
        masked(Operator::ChangeValue, "/size"),
        None,
        "a detected mutation is not masked"
    );
    assert_eq!(
        matrix.undetected_operators(),
        [
            Operator::DeleteKey,
            Operator::ChangeValue,
            Operator::ChangeType,
            Operator::ChangeLength
        ]
    );
}

#[test]
fn a_mutation_inside_a_debt_group_moves_its_count_and_is_detected() {
    let reference = json!({"rows": [1, 2, 3]});
    let port = json!({"rows": [1, 2, 4]});
    let mut tally = Tally::detached("crates/x/tests/corpus.json", []);
    tally.record("s", &reference, &port, vec!["/rows/2".to_owned()]);
    let declared = BTreeMap::from([(("s".to_owned(), "/rows/*".to_owned()), 1)]);
    let debt = Debt::of("crates/x/tests/corpus.json", "test:", declared);
    let matrix = tally.sensitivity_matrix(&debt, None);
    let detected = |operator: Operator, pointer: &str| {
        matrix.operators[&operator]
            .iter()
            .find(|mutation| mutation.pointer == pointer)
            .map(|mutation| mutation.detected)
    };
    // The group still holds one difference: the debt cannot tell.
    assert_eq!(detected(Operator::ChangeValue, "/rows/2"), Some(false));
    assert_eq!(
        matrix.operators[&Operator::ChangeValue]
            .iter()
            .find(|mutation| mutation.pointer == "/rows/2")
            .and_then(|mutation| mutation.masked_by.clone())
            .as_deref(),
        Some("debt s /rows/*")
    );
    // A second difference in the group moves its count.
    assert_eq!(detected(Operator::ChangeValue, "/rows/0"), Some(true));
    assert_eq!(detected(Operator::DeleteKey, "/rows"), Some(true));
}

#[test]
fn a_recorded_difference_list_the_differ_would_not_find_is_refused() {
    let mut tally = Tally::detached("crates/x/tests/corpus.json", []);
    let refused = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        tally.record("s", &json!({"a": 1}), &json!({"a": 2}), Vec::new());
    }));
    assert!(refused.is_err(), "a hidden difference was recorded");

    let mut tally = Tally::detached("crates/x/tests/corpus.json", []);
    tally.differ(|_, _| Vec::new());
    tally.record("s", &json!({"a": 1}), &json!({"a": 2}), Vec::new());
}

#[test]
fn sampling_is_deterministic_and_bounded() {
    let reference = json!({"rows": (0..100).collect::<Vec<_>>()});
    let mut tally = Tally::detached("crates/x/tests/corpus.json", []);
    tally.record("s", &reference, &reference, Vec::new());
    let debt = Debt::of("crates/x/tests/corpus.json", "test:", BTreeMap::new());
    let first = tally.sensitivity_matrix(&debt, Some(5));
    let second = tally.sensitivity_matrix(&debt, Some(5));
    for operator in Operator::ALL {
        let pointers = |matrix: &Matrix| -> Vec<String> {
            matrix
                .operators
                .get(&operator)
                .into_iter()
                .flatten()
                .map(|mutation| mutation.pointer.clone())
                .collect()
        };
        assert!(pointers(&first).len() <= 5);
        assert_eq!(pointers(&first), pointers(&second));
    }
    assert_eq!(first.sites[&Operator::DropElement], 100);
    assert!((first.rate() - 1.0).abs() < f64::EPSILON);
}
