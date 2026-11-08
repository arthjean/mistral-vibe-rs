//! The comparison and ledger semantics every replay runs on: typed prose,
//! segment-exact pointers, bounded entries.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use super::{
    Band, Class, Divergence, Entry, FieldDivergence, OwnProse, PatternDivergence, assert_ledger,
    count_problems, differences, is_top_level, own_row_gap_admitted, pattern_is_top_level,
    pointer_ends_with, pointer_matches, pointer_within,
};

/// A corpus the manifest lists, which [`assert_ledger`] resolves a row and a
/// commit from. Row 15 is what it measures.
const LISTED: &str = "crates/vibe-app-server/tests/review-parity/corpus.json";

fn digest(length: u64, sha256: &str) -> Value {
    json!({"prose": length, "sha256": sha256})
}

fn own_words(pointer: &'static str) -> OwnProse {
    OwnProse {
        scenarios: &["*"],
        pointer,
        count: 1,
        band: Band::DEFAULT,
        directives: None,
        row: "15",
        accepted: "Several unknown review parameters are reported in key order",
        reason: "a test entry",
    }
}

#[test]
fn two_digests_that_differ_are_one_difference_at_the_digest() {
    let reference = json!({"error": {"message": digest(40, "aa"), "code": -32602}});
    let port = json!({"error": {"message": digest(38, "bb"), "code": -32602}});
    assert_eq!(differences(&reference, &port), ["/error/message"]);
    assert!(differences(&reference, &reference).is_empty());
}

#[test]
fn a_digest_against_plain_text_is_a_difference_at_its_pointer() {
    let reference = json!({"title": digest(12, "aa")});
    let port = json!({"title": "Untitled"});
    assert_eq!(differences(&reference, &port), ["/title"]);
}

#[test]
fn an_own_words_entry_admits_a_rewording_within_its_band() {
    let entry = own_words("/error/message");
    assert!(entry.covers("any/scenario", "/error/message"));
    assert_eq!(
        entry.admit(Some(&digest(40, "aa")), Some(&digest(38, "bb"))),
        Ok(())
    );
}

#[test]
fn an_own_words_entry_refuses_a_length_outside_its_band_and_names_it() {
    let entry = own_words("/error/message");
    let refusal = entry
        .admit(Some(&digest(40, "aa")), Some(&digest(81, "bb")))
        .expect_err("twice the length and one more is outside the default band");
    assert!(
        refusal.contains("outside the band of 0.5 to 2 times") && refusal.contains("40"),
        "{refusal}"
    );
    assert!(
        entry
            .admit(Some(&digest(40, "aa")), Some(&digest(19, "bb")))
            .is_err()
    );
}

#[test]
fn an_own_words_entry_refuses_a_structural_difference() {
    let entry = own_words("/title");
    assert!(
        entry
            .admit(Some(&digest(12, "aa")), Some(&json!("Untitled")))
            .is_err()
    );
    assert!(entry.admit(Some(&digest(12, "aa")), None).is_err());
}

#[test]
fn a_prefix_matches_on_segment_boundaries_only() {
    assert!(pointer_within("/display/summary", "/display/summary"));
    assert!(pointer_within("/display/summary", "/display/summary/0"));
    assert!(pointer_within("/display/summary/", "/display/summary/0"));
    assert!(!pointer_within(
        "/display/summary",
        "/display/summaryDetail"
    ));
    assert!(pointer_ends_with("/message", "/error/message"));
    assert!(!pointer_ends_with("/message", "/error/errorMessage"));
}

#[test]
fn a_divergence_on_a_prefix_does_not_cover_a_sibling_sharing_its_spelling() {
    let entry = Divergence {
        scenarios: &["one"],
        pointer: "/display/summary",
        within: &[],
        count: 1,
        row: "3",
        class: Class::Gap,
        reason: "a test entry",
    };
    assert!(entry.covers("one", "/display/summary"));
    assert!(!entry.covers("one", "/display/summaryDetail"));
    assert!(!entry.covers("two", "/display/summary"));
}

#[test]
fn a_whole_top_level_observation_needs_its_sub_pointers_listed() {
    assert!(is_top_level("/requests/"));
    assert!(is_top_level("/steps"));
    assert!(!is_top_level("/steps/0/turn"));
    let broad = Divergence {
        scenarios: &["one"],
        pointer: "/requests/",
        within: &[],
        count: 1,
        row: "3",
        class: Class::Gap,
        reason: "a test entry",
    };
    assert!(
        broad
            .problems()
            .iter()
            .any(|problem| problem.contains("whole top-level observation"))
    );
    let narrowed = Divergence {
        within: &["/*/conversation/*/content"],
        ..broad
    };
    assert!(narrowed.problems().is_empty());
    assert!(pattern_is_top_level("/steps/**"));
    assert!(pattern_is_top_level("/**"));
    assert!(!pattern_is_top_level("/steps/*/turn/**"));
    assert!(
        !pattern_is_top_level("/stdout"),
        "a pattern without `**` names one value, the digest an argument vector printed"
    );
    assert!(own_words("/stdout").problems().is_empty());
    assert!(
        !own_words("/requests/**").problems().is_empty(),
        "an own-words pattern over a whole observation is refused too"
    );
    assert!(narrowed.covers("one", "/requests/2/conversation/4/content"));
    assert!(!narrowed.covers("one", "/requests/2/model"));
}

#[test]
fn a_double_star_matches_one_or_more_segments_anywhere() {
    assert!(pointer_matches(
        "/**/description",
        "/requests/0/tools/3/description"
    ));
    assert!(pointer_matches("/**/description", "/a/description"));
    assert!(!pointer_matches("/**/description", "/description"));
    assert!(pointer_matches("/steps/**", "/steps/0/turn"));
    assert!(!pointer_matches("/steps/**", "/steps"));
    assert!(pointer_matches("/*/events/*/x", "/0/events/3/x"));
}

fn covered(pairs: &[(&str, usize)]) -> BTreeMap<String, usize> {
    pairs
        .iter()
        .map(|(scenario, count)| ((*scenario).to_owned(), *count))
        .collect()
}

#[test]
fn an_entry_that_covers_another_number_of_differences_than_it_declares_is_reported() {
    let divergence = Divergence {
        scenarios: &["one", "two"],
        pointer: "/a",
        within: &[],
        count: 2,
        row: "3",
        class: Class::Gap,
        reason: "a test entry",
    };
    let field = FieldDivergence {
        suffix: "/b",
        count: 2,
        row: "3",
        class: Class::Gap,
        reason: "a test entry",
    };
    let entries: [&dyn Entry; 2] = [&divergence, &field];
    let (stale, miscounted) = count_problems(
        &entries,
        &[
            covered(&[("one", 2), ("two", 3)]),
            covered(&[("one", 2), ("three", 2)]),
        ],
    );
    assert!(stale.is_empty(), "{stale:?}");
    assert_eq!(
        miscounted,
        ["/a declares 2 differences in two and covered 3"],
        "the field entry covered 2 in each scenario it covered, as declared"
    );
    let (stale, miscounted) =
        count_problems(&entries, &[covered(&[("one", 2)]), covered(&[("one", 4)])]);
    assert_eq!(stale, ["two /a"]);
    assert_eq!(
        miscounted,
        ["/b declares 2 differences in one and covered 4"]
    );
}

#[test]
fn a_field_entry_counts_per_scenario_so_a_shift_between_scenarios_is_caught() {
    let field = FieldDivergence {
        suffix: "/b",
        count: 2,
        row: "3",
        class: Class::Gap,
        reason: "a test entry",
    };
    let entries: [&dyn Entry; 1] = [&field];
    // Four differences in all, as two scenarios of two would give, but one
    // scenario regressed to three while the other improved to one.
    let (stale, miscounted) = count_problems(&entries, &[covered(&[("one", 3), ("two", 1)])]);
    assert!(stale.is_empty(), "{stale:?}");
    assert_eq!(
        miscounted,
        [
            "/b declares 2 differences in one and covered 3",
            "/b declares 2 differences in two and covered 1",
        ]
    );
    let (stale, _) = count_problems(&entries, &[covered(&[("one", 0)])]);
    assert_eq!(stale, ["/b"]);
}

#[test]
fn an_own_words_entry_no_difference_exercises_is_stale() {
    let entry = own_words("/error/message");
    let entries: [&dyn Entry; 1] = [&entry];
    let (stale, miscounted) = count_problems(&entries, &[covered(&[])]);
    assert_eq!(stale, ["own words * /error/message"]);
    assert!(miscounted.is_empty(), "{miscounted:?}");
    let (stale, miscounted) = count_problems(&entries, &[covered(&[("a", 1)])]);
    assert!(stale.is_empty() && miscounted.is_empty());
}

#[test]
fn a_pattern_divergence_names_a_scorecard_row() {
    let entry = PatternDivergence {
        scenario: "*",
        pointer: "/x",
        count: 1,
        row: "99",
        class: Class::Gap,
        reason: "a test entry",
    };
    let corpus: Value = serde_json::from_str(
        &std::fs::read_to_string(super::repository().join(LISTED)).expect("the corpus reads"),
    )
    .expect("the corpus parses");
    let recorded = super::scenarios(&corpus);
    let failure = std::panic::catch_unwind(|| {
        assert_ledger(LISTED, &recorded, [&entry as &dyn Entry]);
    })
    .expect_err("row 99 is not a row of docs/parity.md");
    let message = failure
        .downcast_ref::<String>()
        .cloned()
        .unwrap_or_default();
    assert!(message.contains("names row 99"), "{message}");
}

#[test]
fn an_own_words_entry_audited_at_another_commit_than_the_corpus_fails() {
    let entry = OwnProse {
        directives: Some("000000000000"),
        ..own_words("/x")
    };
    let corpus: Value = serde_json::from_str(
        &std::fs::read_to_string(super::repository().join(LISTED)).expect("the corpus reads"),
    )
    .expect("the corpus parses");
    let recorded = super::scenarios(&corpus);
    let failure = std::panic::catch_unwind(|| {
        assert_ledger(LISTED, &recorded, [&entry as &dyn Entry]);
    })
    .expect_err("the corpus is captured from the pin, not from 0000000");
    let message = failure
        .downcast_ref::<String>()
        .cloned()
        .unwrap_or_default();
    assert!(
        message.contains("was audited against 000000000000"),
        "{message}"
    );
}

#[test]
fn an_audit_written_with_the_full_hash_is_refused() {
    let corpus: Value = serde_json::from_str(
        &std::fs::read_to_string(super::repository().join(LISTED)).expect("the corpus reads"),
    )
    .expect("the corpus parses");
    let recorded = super::scenarios(&corpus);
    let full = crate::parity::REFERENCE_COMMIT;
    let entry = OwnProse {
        directives: Some(full),
        ..own_words("/x")
    };
    assert!(
        std::panic::catch_unwind(|| {
            assert_ledger(LISTED, &recorded, [&entry as &dyn Entry]);
        })
        .is_err(),
        "an audit names the commit by its 12-character abbreviation"
    );
}

#[test]
fn a_gap_of_the_row_the_corpus_measures_is_not_a_ledger_entry() {
    let entry = PatternDivergence {
        scenario: "*",
        pointer: "/x",
        count: 1,
        row: "15",
        class: Class::Gap,
        reason: "a test entry",
    };
    let corpus: Value = serde_json::from_str(
        &std::fs::read_to_string(super::repository().join(LISTED)).expect("the corpus reads"),
    )
    .expect("the corpus parses");
    let recorded = super::scenarios(&corpus);
    let refused = std::panic::catch_unwind(|| {
        assert_ledger(LISTED, &recorded, [&entry as &dyn Entry]);
    })
    .is_err();
    // While a re-pin is open, a corpus recaptured at its target may count a
    // gap of its own row; outside one, or at the previous commit, it may not.
    let commit =
        crate::parity::manifest::recorded_commit(&corpus).expect("the corpus records a commit");
    assert_eq!(
        refused,
        !own_row_gap_admitted(crate::parity::PREVIOUS_COMMIT, commit),
        "row 15 is what the review corpus measures"
    );
}

#[test]
fn a_gap_of_the_measured_row_is_admitted_only_at_the_target_of_an_open_transition() {
    const LEFT: &str = "fedcba9876543210fedcba9876543210fedcba98";
    let target = crate::parity::REFERENCE_COMMIT;
    assert!(own_row_gap_admitted(LEFT, target));
    assert!(!own_row_gap_admitted("", target), "no transition is open");
    assert!(
        !own_row_gap_admitted(LEFT, LEFT),
        "a corpus still at the previous commit keeps the rule"
    );
}
