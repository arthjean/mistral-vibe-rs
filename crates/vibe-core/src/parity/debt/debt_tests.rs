//! The ratchet of a measured debt, and the shape its file has to keep.

use std::collections::BTreeMap;

use serde_json::json;

use super::{Debt, document_problems, generalize, groups, path_of};

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
        "schemaVersion": 1,
        "corpus": LISTED,
        "measuredAgainst": MEASURED,
        "tallies": {"sample parity:": {"flows/a": {"/*/result": 2}}},
    });
    assert!(document_problems(&sound, LISTED, MEASURED).is_empty());
    assert_eq!(
        groups(&sound, "sample parity:"),
        BTreeMap::from([(("flows/a".to_owned(), "/*/result".to_owned()), 2)])
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

    let empty = json!({
        "schemaVersion": 1,
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
