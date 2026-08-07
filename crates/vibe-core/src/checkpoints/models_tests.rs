//! The domain vocabulary, including the spellings that reach a client.

use std::collections::BTreeMap;

use super::lines::FileState;
use super::models::{
    CheckpointError, Decision, HunkAnchor, HunkSide, OpaqueChange, OpaqueReason, Owner, Region,
    RegionId,
};

#[test]
fn a_file_state_answers_for_its_existence_and_its_bytes() {
    assert!(!FileState::absent().exists());
    assert!(!FileState::absent().is_binary());
    assert!(FileState::from_text("").exists());
    assert!(!FileState::from_text("text\n").is_binary());
    assert!(FileState::from_bytes(b"before\0after".to_vec()).is_binary());
}

#[test]
fn a_region_spans_are_half_open() {
    let region = Region {
        baseline_start: 3,
        baseline_lines: vec!["a\n".to_owned(), "b\n".to_owned()],
        current_start: 5,
        current_lines: vec!["c\n".to_owned()],
    };

    assert_eq!(region.baseline_end(), 5);
    assert_eq!(region.current_end(), 6);
}

#[test]
fn an_opaque_change_keeps_both_sides_so_a_revert_can_restore() {
    let change = OpaqueChange {
        reason: OpaqueReason::Missing,
        baseline: FileState::from_text("was here\n"),
        current: FileState::absent(),
    };

    assert_eq!(change.baseline, FileState::from_text("was here\n"));
    assert!(!change.current.exists());
    assert_eq!(
        serde_json::to_string(&OpaqueReason::BinaryOrUndecodable).unwrap(),
        "\"binary_or_undecodable\""
    );
    assert_eq!(
        serde_json::to_string(&OpaqueReason::Missing).unwrap(),
        "\"missing\""
    );
}

#[test]
fn a_region_identity_orders_by_its_pair_and_keys_a_map() {
    let mut sorted = vec![
        RegionId::new(4, 0),
        RegionId::new(2, 3),
        RegionId::new(2, 0),
    ];
    sorted.sort_unstable();
    assert_eq!(
        sorted,
        vec![
            RegionId::new(2, 0),
            RegionId::new(2, 3),
            RegionId::new(4, 0)
        ]
    );

    let mut keyed = BTreeMap::new();
    keyed.insert(RegionId::new(2, 0), "first");
    keyed.insert(RegionId::new(2, 0), "replaced");
    assert_eq!(keyed.get(&RegionId::new(2, 0)), Some(&"replaced"));
    assert_eq!(keyed.len(), 1);
    assert_eq!(RegionId::new(2, 1).to_string(), "2.1");
}

#[test]
fn an_owner_says_on_the_wire_which_of_the_two_it_is() {
    assert_eq!(
        serde_json::to_string(&Owner::Agent { turn_id: 7 }).unwrap(),
        r#"{"kind":"agent","turnId":7}"#
    );
    assert_eq!(
        serde_json::to_string(&Owner::Manual { index: 1 }).unwrap(),
        r#"{"kind":"manual","index":1}"#
    );
    assert_ne!(Owner::Agent { turn_id: 1 }, Owner::Manual { index: 1 });
}

#[test]
fn a_decision_serializes_as_the_three_lowercase_names() {
    assert_eq!(
        serde_json::to_string(&Decision::Pending).unwrap(),
        "\"pending\""
    );
    assert_eq!(serde_json::to_string(&Decision::Keep).unwrap(), "\"keep\"");
    assert_eq!(
        serde_json::to_string(&Decision::Revert).unwrap(),
        "\"revert\""
    );
}

#[test]
fn a_pending_decision_is_refused_rather_than_defaulted() {
    assert_eq!(Decision::Keep.as_keep(), Ok(true));
    assert_eq!(Decision::Revert.as_keep(), Ok(false));
    assert_eq!(
        Decision::Pending.as_keep(),
        Err(CheckpointError::PendingDecision)
    );
}

#[test]
fn a_hunk_anchor_carries_a_side_a_zero_based_line_and_its_targets() {
    let anchor = HunkAnchor {
        side: HunkSide::Deletions,
        line: 0,
        regions: vec![RegionId::new(2, 0), RegionId::new(4, 1)],
    };

    assert_eq!(anchor.regions.len(), 2);
    assert_eq!(
        serde_json::to_string(&anchor.side).unwrap(),
        "\"deletions\""
    );
    assert_eq!(
        serde_json::to_string(&HunkSide::Additions).unwrap(),
        "\"additions\""
    );
}

#[test]
fn an_unknown_region_names_the_path_and_the_region() {
    let error = CheckpointError::UnknownRegion {
        path: "src/main.rs".to_owned(),
        region: RegionId::new(4, 2),
    };

    let message = error.to_string();
    assert!(message.contains("src/main.rs"), "{message}");
    assert!(message.contains("4.2"), "{message}");
}
