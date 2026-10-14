//! The harness decision a pair of flags resolves to.

use serde_json::{Value, json};

use super::{HarnessSelection, HarnessSelectionSource};

#[test]
fn legacy_wins_over_everything() {
    for experimental in [false, true] {
        let selection = HarnessSelection::resolve(experimental, true);
        assert_eq!(selection.source, HarnessSelectionSource::FlagLegacy);
        assert!(!selection.use_unified);
    }
}

#[test]
fn the_flag_selects_the_unified_harness() {
    let selection = HarnessSelection::resolve(true, false);
    assert_eq!(selection.source, HarnessSelectionSource::Flag);
    assert!(selection.use_unified);
}

#[test]
fn no_flag_is_the_default_unified_harness() {
    let selection = HarnessSelection::resolve(false, false);
    assert_eq!(selection.source, HarnessSelectionSource::Default);
    assert!(selection.use_unified);
}

#[test]
fn config_read_publishes_both_fields_in_the_wire_spelling() {
    let fields = HarnessSelection::resolve(false, true).config_read_fields();
    assert_eq!(fields[0], ("startupIssue", Value::Null));
    assert_eq!(fields[1], ("harnessSelectionSource", json!("flag-legacy")));
    let fields = HarnessSelection::resolve(true, false).config_read_fields();
    assert_eq!(fields[0].1, Value::Null);
    assert_eq!(fields[1].1, json!("flag"));
}
