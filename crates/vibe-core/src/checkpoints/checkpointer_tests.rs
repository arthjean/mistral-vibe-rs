//! What the log accepts, what it refuses, and what a failure undoes.

use super::checkpointer::Checkpointer;
use super::lines::FileState;
use super::models::{CheckpointError, Decision, Owner, RegionId};

const PATH: &str = "f";

fn text(value: &str) -> FileState {
    FileState::from_text(value)
}

fn turn_from(log: &mut Checkpointer, turn_id: u64, before: FileState, after: FileState) {
    log.begin_turn(turn_id).unwrap();
    log.record_pre_edit(PATH, before).unwrap();
    log.record_post_edit(PATH, after).unwrap();
    log.seal_turn();
}

fn ids(log: &Checkpointer) -> Vec<RegionId> {
    log.history()
        .regions(PATH)
        .into_iter()
        .map(|region| region.region_id)
        .collect()
}

fn decisions(log: &Checkpointer) -> Vec<Decision> {
    log.history()
        .regions(PATH)
        .into_iter()
        .map(|region| region.decision)
        .collect()
}

// -- US-123: the turn lifecycle ----------------------------------------------

#[test]
fn a_turn_opens_with_nothing_recorded_against_it() {
    let mut log = Checkpointer::new();
    assert!(!log.has_open_turn());

    log.begin_turn(7).unwrap();

    assert!(log.has_open_turn());
    log.seal_turn();
    assert!(!log.has_open_turn());
    assert!(
        log.history().regions(PATH).is_empty(),
        "a turn that touched nothing appends no edit"
    );
}

#[test]
fn beginning_a_turn_while_one_is_open_is_refused_and_changes_nothing() {
    let mut log = Checkpointer::new();
    log.begin_turn(1).unwrap();
    log.record_pre_edit(PATH, text("a\n")).unwrap();

    assert_eq!(log.begin_turn(2), Err(CheckpointError::TurnAlreadyOpen));

    log.record_post_edit(PATH, text("b\n")).unwrap();
    log.seal_turn();
    let history = log.history();
    let regions = history.regions(PATH);
    assert_eq!(regions.len(), 1);
    assert_eq!(regions[0].owner, Owner::Agent { turn_id: 1 });
}

#[test]
fn the_first_pre_edit_recording_of_a_path_owns_the_turn() {
    let mut log = Checkpointer::new();
    log.begin_turn(1).unwrap();
    log.record_pre_edit(PATH, text("first\n")).unwrap();
    log.record_pre_edit(PATH, text("second\n")).unwrap();
    log.record_post_edit(PATH, text("third\n")).unwrap();
    log.seal_turn();

    assert_eq!(log.history().original(PATH), text("first\n"));
    assert_eq!(log.history().content(PATH), text("third\n"));
}

#[test]
fn recording_outside_a_turn_is_refused() {
    let mut log = Checkpointer::new();

    assert_eq!(
        log.record_pre_edit(PATH, text("a\n")),
        Err(CheckpointError::NoOpenTurn {
            operation: "a pre-edit state"
        })
    );
    assert_eq!(
        log.record_post_edit(PATH, text("a\n")),
        Err(CheckpointError::NoOpenTurn {
            operation: "a post-edit state"
        })
    );
    assert!(log.history().regions(PATH).is_empty());
}

#[test]
fn sealing_appends_one_edit_per_changed_path_and_nothing_for_the_rest() {
    let mut log = Checkpointer::new();
    log.begin_turn(1).unwrap();
    log.record_pre_edit("changed", text("a\n")).unwrap();
    log.record_pre_edit("untouched", text("same\n")).unwrap();
    log.record_post_edit("changed", text("b\n")).unwrap();
    log.record_post_edit("untouched", text("same\n")).unwrap();
    log.seal_turn();

    let history = log.history();
    assert_eq!(history.regions("changed").len(), 1);
    assert!(
        history.regions("untouched").is_empty(),
        "a path whose content did not move appends no edit"
    );
    assert_eq!(history.content("untouched"), text("same\n"));
}

#[test]
fn a_path_never_recorded_after_the_edit_defaults_to_its_own_before_state() {
    let mut log = Checkpointer::new();
    log.begin_turn(1).unwrap();
    log.record_pre_edit(PATH, text("a\n")).unwrap();
    log.seal_turn();

    assert!(log.history().regions(PATH).is_empty());
    assert_eq!(log.history().content(PATH), text("a\n"));
}

#[test]
fn sealing_with_no_open_turn_does_nothing() {
    let mut log = Checkpointer::new();
    turn_from(&mut log, 1, text("a\n"), text("b\n"));
    let before = ids(&log);

    log.seal_turn();
    log.seal_turn();

    assert_eq!(ids(&log), before);
    assert!(!log.has_open_turn());
}

#[test]
fn a_failure_inside_the_atomic_scope_restores_the_log() {
    let mut log = Checkpointer::new();
    turn_from(&mut log, 1, text("a\nb\n"), text("A\nb\n"));
    let region = ids(&log)[0];
    let before = decisions(&log);

    let outcome: Result<(), &str> = log.atomic(|inner| {
        inner.decide_region(PATH, region, Decision::Revert).ok();
        Err("the disk write failed")
    });

    assert_eq!(outcome, Err("the disk write failed"));
    assert_eq!(decisions(&log), before);
    assert_eq!(log.history().content(PATH), text("A\nb\n"));

    // The sequence counter came back too, so the next append is numbered where
    // it would have been had the rolled-back one never happened.
    turn_from(&mut log, 2, text("A\nb\n"), text("A\nB\n"));
    assert_eq!(ids(&log), vec![RegionId::new(2, 0), RegionId::new(4, 0)]);
}

#[test]
fn a_successful_atomic_scope_keeps_what_it_did() {
    let mut log = Checkpointer::new();
    turn_from(&mut log, 1, text("a\nb\n"), text("A\nb\n"));
    let region = ids(&log)[0];

    let outcome: Result<(), CheckpointError> =
        log.atomic(|inner| inner.decide_region(PATH, region, Decision::Revert));

    assert_eq!(outcome, Ok(()));
    assert_eq!(decisions(&log), vec![Decision::Revert]);
}

#[test]
fn clearing_resets_the_log_and_everything_counted_against_it() {
    let mut log = Checkpointer::new();
    turn_from(&mut log, 1, text("a\n"), text("b\n"));
    log.begin_turn(2).unwrap();

    log.clear();

    assert!(!log.has_open_turn());
    assert!(log.history().regions(PATH).is_empty());
    turn_from(&mut log, 1, text("a\n"), text("b\n"));
    assert_eq!(
        ids(&log),
        vec![RegionId::new(2, 0)],
        "the sequence counter restarted, so identities are numbered from the beginning again"
    );
}

// -- US-126: recording decisions ---------------------------------------------

#[test]
fn keeping_a_region_records_the_pending_regions_it_was_built_on() {
    let mut log = Checkpointer::new();
    turn_from(&mut log, 1, text(""), text("one\ntwo\n"));
    log.begin_turn(2).unwrap();
    log.record_pre_edit(PATH, text("one\ntwo\n")).unwrap();
    log.record_post_edit(PATH, text("one\nTWO\n")).unwrap();
    log.seal_turn();
    let region_ids = ids(&log);

    log.decide_region(PATH, region_ids[1], Decision::Keep)
        .unwrap();

    assert_eq!(decisions(&log), vec![Decision::Keep, Decision::Keep]);
}

#[test]
fn reverting_a_region_records_only_that_region() {
    let mut log = Checkpointer::new();
    turn_from(&mut log, 1, text(""), text("one\ntwo\n"));
    log.begin_turn(2).unwrap();
    log.record_pre_edit(PATH, text("one\ntwo\n")).unwrap();
    log.record_post_edit(PATH, text("one\nTWO\n")).unwrap();
    log.seal_turn();
    let region_ids = ids(&log);

    log.decide_region(PATH, region_ids[0], Decision::Revert)
        .unwrap();

    assert_eq!(decisions(&log), vec![Decision::Revert, Decision::Revert]);
    // Nothing was recorded against the dependent: truncating the decision would
    // free it again, which is what dragging at read buys.
    assert_eq!(log.history().content(PATH), text(""));
}

#[test]
fn deciding_a_scope_touches_only_that_owners_pending_regions() {
    let mut log = Checkpointer::new();
    turn_from(&mut log, 1, text("a\nb\nc\n"), text("A\nb\nC\n"));
    log.begin_turn(2).unwrap();
    log.record_pre_edit(PATH, text("A\nb\nC\n")).unwrap();
    log.record_post_edit(PATH, text("A\nB\nC\n")).unwrap();
    log.seal_turn();

    log.decide_scope(PATH, Owner::Agent { turn_id: 2 }, Decision::Revert)
        .unwrap();

    assert_eq!(
        decisions(&log),
        vec![Decision::Pending, Decision::Pending, Decision::Revert]
    );
    assert_eq!(log.history().content(PATH), text("A\nb\nC\n"));
}

#[test]
fn deciding_a_file_decides_every_still_pending_region() {
    let mut log = Checkpointer::new();
    turn_from(&mut log, 1, text("a\nb\nc\n"), text("A\nb\nC\n"));
    let region_ids = ids(&log);
    log.decide_region(PATH, region_ids[0], Decision::Revert)
        .unwrap();

    log.decide_file(PATH, Decision::Keep).unwrap();

    assert_eq!(
        decisions(&log),
        vec![Decision::Revert, Decision::Keep],
        "the already-reverted region is untouched"
    );
}

#[test]
fn deciding_an_unknown_region_is_refused_and_changes_nothing() {
    let mut log = Checkpointer::new();
    turn_from(&mut log, 1, text("a\n"), text("b\n"));
    let unknown = RegionId::new(99, 3);

    assert_eq!(
        log.decide_region(PATH, unknown, Decision::Revert),
        Err(CheckpointError::UnknownRegion {
            path: PATH.to_owned(),
            region: unknown,
        })
    );
    assert_eq!(decisions(&log), vec![Decision::Pending]);
}

#[test]
fn deciding_while_a_turn_is_open_is_refused() {
    let mut log = Checkpointer::new();
    turn_from(&mut log, 1, text("a\n"), text("b\n"));
    let region = ids(&log)[0];
    log.begin_turn(2).unwrap();

    assert_eq!(
        log.decide_region(PATH, region, Decision::Revert),
        Err(CheckpointError::DecisionDuringTurn)
    );
    assert_eq!(
        log.decide_file(PATH, Decision::Revert),
        Err(CheckpointError::DecisionDuringTurn)
    );
    assert_eq!(
        log.decide_scope(PATH, Owner::Agent { turn_id: 1 }, Decision::Revert),
        Err(CheckpointError::DecisionDuringTurn)
    );
    assert_eq!(decisions(&log), vec![Decision::Pending]);
}

#[test]
fn a_pending_decision_is_refused_wherever_it_is_passed() {
    let mut log = Checkpointer::new();
    turn_from(&mut log, 1, text("a\n"), text("b\n"));
    let region = ids(&log)[0];

    assert_eq!(
        log.decide_region(PATH, region, Decision::Pending),
        Err(CheckpointError::PendingDecision)
    );
    assert_eq!(
        log.decide_file(PATH, Decision::Pending),
        Err(CheckpointError::PendingDecision)
    );
    assert_eq!(
        log.decide_scope(PATH, Owner::Agent { turn_id: 1 }, Decision::Pending),
        Err(CheckpointError::PendingDecision)
    );
}
