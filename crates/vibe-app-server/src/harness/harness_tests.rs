//! The harness decision a pair of flags resolves to.

use serde_json::{Value, json};

use super::{HarnessSelection, HarnessSelectionSource};

#[test]
fn legacy_wins_over_everything_and_raises_nothing() {
    for experimental in [false, true] {
        let selection = HarnessSelection::resolve(experimental, true);
        assert_eq!(selection.source, HarnessSelectionSource::FlagLegacy);
        assert!(selection.startup_issue.is_none());
    }
}

/// `--experimental-harness` keeps `flag` as its source even though the backend
/// it asks for is absent, which is what `HarnessProcess` publishes after its
/// own fallback.
#[test]
fn the_unified_request_falls_back_with_an_issue_on_the_flag() {
    let selection = HarnessSelection::resolve(true, false);
    assert_eq!(selection.source, HarnessSelectionSource::Flag);
    let issue = selection.startup_issue.expect("the fallback is reported");
    assert_eq!(issue.file, "--experimental-harness");
    assert!(!issue.message.is_empty());
}

#[test]
fn no_flag_is_the_default_legacy_harness() {
    assert_eq!(
        HarnessSelection::default().source,
        HarnessSelectionSource::Default
    );
    assert!(HarnessSelection::default().startup_issue.is_none());
}

#[test]
fn config_read_publishes_both_fields_in_the_wire_spelling() {
    let fields = HarnessSelection::resolve(false, true).config_read_fields();
    assert_eq!(fields[0], ("startupIssue", Value::Null));
    assert_eq!(fields[1], ("harnessSelectionSource", json!("flag-legacy")));
    let fields = HarnessSelection::resolve(true, false).config_read_fields();
    assert_eq!(fields[0].1["file"], json!("--experimental-harness"));
    assert_eq!(fields[1].1, json!("flag"));
}
