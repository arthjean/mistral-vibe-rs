//! The harness decision a pair of flags and the rollout resolve to.

use serde_json::{Value, json};

use super::{HarnessSelection, HarnessSelectionSource};

#[test]
fn legacy_wins_over_everything() {
    for experimental in [false, true] {
        for rollout in [None, Some("unified")] {
            let selection = HarnessSelection::resolve_with_rollout(experimental, true, rollout);
            assert_eq!(selection.source, HarnessSelectionSource::FlagLegacy);
            assert!(!selection.use_unified);
        }
    }
}

#[test]
fn the_flag_selects_the_unified_mode() {
    let selection = HarnessSelection::resolve(true, false);
    assert_eq!(selection.source, HarnessSelectionSource::Flag);
    assert!(selection.use_unified);
}

#[test]
fn only_the_unified_rollout_variant_selects_it() {
    let selection = HarnessSelection::resolve_with_rollout(false, false, Some("unified"));
    assert_eq!(selection.source, HarnessSelectionSource::Rollout);
    assert!(selection.use_unified);
    let selection = HarnessSelection::resolve_with_rollout(false, false, Some("legacy"));
    assert_eq!(selection.source, HarnessSelectionSource::Default);
    assert!(!selection.use_unified);
}

#[test]
fn the_rollout_is_read_from_any_recent_cache_entry() {
    let home = tempfile::tempdir().unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let payload = json!({
        "features": {"vibe_cli_unified_harness_rollout": {"defaultValue": "unified"}}
    });
    std::fs::write(
        home.path().join("experiment_eval_cache.json"),
        json!({
            "stale": {"stored_at_timestamp": 1, "payload": {"features": {}}},
            "fresh": {"stored_at_timestamp": now, "payload": payload},
        })
        .to_string(),
    )
    .unwrap();
    let selection = HarnessSelection::for_launch(false, false, home.path());
    assert_eq!(selection.source, HarnessSelectionSource::Rollout);
}

#[test]
fn no_flag_is_the_default_legacy_harness() {
    assert_eq!(
        HarnessSelection::default().source,
        HarnessSelectionSource::Default
    );
    assert!(!HarnessSelection::default().use_unified);
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
