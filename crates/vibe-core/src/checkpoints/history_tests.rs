//! What the read model answers, scenario by scenario.
//!
//! Every expectation here was taken from the pinned reference engine driven
//! over the same scenario, not from reading the Rust back to itself. The
//! scenarios are built through the log's own lifecycle rather than by planting
//! events, so a test that passes proves the path a turn actually takes.

use super::checkpointer::Checkpointer;
use super::lines::FileState;
use super::models::{Change, Decision, HunkAnchor, HunkSide, OpaqueReason, Owner, RegionId};

const PATH: &str = "f";

fn text(value: &str) -> FileState {
    FileState::from_text(value)
}

/// One turn rewriting [`PATH`] from `before` to `after`.
fn turn_from(log: &mut Checkpointer, turn_id: u64, before: FileState, after: FileState) {
    log.begin_turn(turn_id).unwrap();
    log.record_pre_edit(PATH, before).unwrap();
    log.record_post_edit(PATH, after).unwrap();
    log.seal_turn();
}

/// One turn rewriting [`PATH`] to `after`, starting from what the log projects.
fn turn(log: &mut Checkpointer, turn_id: u64, after: FileState) {
    let before = log.history().content(PATH);
    turn_from(log, turn_id, before, after);
}

fn agent(turn_id: u64) -> Owner {
    Owner::Agent { turn_id }
}

fn ids(log: &Checkpointer) -> Vec<RegionId> {
    log.history()
        .regions(PATH)
        .into_iter()
        .map(|region| region.region_id)
        .collect()
}

/// One region's identity, what it was built on and the decision in force,
/// flattened so an expectation reads as the reference printed it.
type RegionSummary = ((u64, usize), Vec<(u64, usize)>, Decision);

/// One anchor, flattened the same way.
type AnchorSummary = (HunkSide, usize, Vec<(u64, usize)>);

/// Every region's identity, dependencies and decision, in log order.
fn summary(log: &Checkpointer) -> Vec<RegionSummary> {
    log.history()
        .regions(PATH)
        .into_iter()
        .map(|region| {
            (
                (region.region_id.version_index, region.region_id.ordinal),
                region
                    .depends_on
                    .iter()
                    .map(|dep| (dep.version_index, dep.ordinal))
                    .collect(),
                region.decision,
            )
        })
        .collect()
}

fn anchors(log: &Checkpointer, owner: Option<Owner>) -> Vec<AnchorSummary> {
    log.history()
        .pending_hunks(PATH, owner)
        .into_iter()
        .map(|anchor: HunkAnchor| {
            (
                anchor.side,
                anchor.line,
                anchor
                    .regions
                    .iter()
                    .map(|region| (region.version_index, region.ordinal))
                    .collect(),
            )
        })
        .collect()
}

// -- US-124: regions and opacity ---------------------------------------------

#[test]
fn a_text_edit_resolves_to_its_non_equal_opcodes_in_order() {
    let mut log = Checkpointer::new();
    turn_from(&mut log, 1, text("a\nb\nc\n"), text("A\nb\nC\n"));

    let history = log.history();
    let regions = history.regions(PATH);
    assert_eq!(regions.len(), 2);
    assert_eq!(regions[0].region_id, RegionId::new(2, 0));
    assert_eq!(regions[1].region_id, RegionId::new(2, 1));
    let Change::Text(first) = &regions[0].change else {
        panic!("the first change is a line hunk");
    };
    assert_eq!(first.baseline_start, 0);
    assert_eq!(first.baseline_lines, vec!["a\n".to_owned()]);
    assert_eq!(first.current_start, 0);
    assert_eq!(first.current_lines, vec!["A\n".to_owned()]);
    let Change::Text(second) = &regions[1].change else {
        panic!("the second change is a line hunk");
    };
    assert_eq!(second.baseline_start, 2);
    assert_eq!(second.current_start, 2);
    assert_eq!(second.baseline_lines, vec!["c\n".to_owned()]);
    assert_eq!(second.current_lines, vec!["C\n".to_owned()]);
}

#[test]
fn a_binary_side_resolves_to_one_whole_file_unit() {
    let mut log = Checkpointer::new();
    turn_from(
        &mut log,
        1,
        text("a\nb\n"),
        FileState::from_bytes(b"\0binary".to_vec()),
    );

    let history = log.history();
    let regions = history.regions(PATH);
    assert_eq!(regions.len(), 1);
    let Change::Opaque(change) = &regions[0].change else {
        panic!("a binary side has no line hunks");
    };
    assert_eq!(change.reason, OpaqueReason::BinaryOrUndecodable);
    assert_eq!(change.baseline, text("a\nb\n"));
    assert_eq!(change.current, FileState::from_bytes(b"\0binary".to_vec()));
}

#[test]
fn an_absent_after_state_resolves_to_a_missing_whole_file_unit() {
    let mut log = Checkpointer::new();
    turn_from(&mut log, 1, text("a\n"), FileState::absent());

    let history = log.history();
    let regions = history.regions(PATH);
    assert_eq!(regions.len(), 1);
    let Change::Opaque(change) = &regions[0].change else {
        panic!("a deletion has no line hunks");
    };
    assert_eq!(change.reason, OpaqueReason::Missing);
}

#[test]
fn an_existence_toggle_with_no_textual_difference_is_one_whole_file_unit() {
    let mut log = Checkpointer::new();
    turn_from(&mut log, 1, FileState::absent(), text(""));

    let history = log.history();
    let regions = history.regions(PATH);
    assert_eq!(regions.len(), 1);
    let Change::Opaque(change) = &regions[0].change else {
        panic!("an empty-file creation has no line hunk to show");
    };
    assert_eq!(change.reason, OpaqueReason::Missing);
}

#[test]
fn a_creation_reports_missing_when_it_has_no_lines_to_diff() {
    // The reference calls a creation opaque only when it has no line structure
    // of its own; a creation with text is an ordinary insert hunk. Both are
    // asserted here, because only the pair says where the boundary is.
    let mut opaque = Checkpointer::new();
    turn_from(
        &mut opaque,
        1,
        FileState::absent(),
        FileState::from_bytes(b"\0binary".to_vec()),
    );
    let history = opaque.history();
    let regions = history.regions(PATH);
    let Change::Opaque(change) = &regions[0].change else {
        panic!("a binary creation has no line hunks");
    };
    assert_eq!(change.reason, OpaqueReason::Missing);

    let mut textual = Checkpointer::new();
    turn_from(&mut textual, 1, FileState::absent(), text("a\nb\nc\n"));
    let history = textual.history();
    let regions = history.regions(PATH);
    assert_eq!(regions.len(), 1);
    let Change::Text(region) = &regions[0].change else {
        panic!("a creation carrying text is an insert hunk");
    };
    assert!(region.baseline_lines.is_empty());
    assert_eq!(region.current_lines.len(), 3);
}

#[test]
fn an_edit_is_diffed_once_per_read_model() {
    let mut log = Checkpointer::new();
    turn_from(&mut log, 1, text("a\n"), text("b\n"));
    turn(&mut log, 2, text("c\n"));

    let history = log.history();
    assert_eq!(history.memoized_edits(), 0);
    let first = history.regions(PATH);
    assert_eq!(history.memoized_edits(), 2);
    let second = history.regions(PATH);
    assert_eq!(
        history.memoized_edits(),
        2,
        "the second read diffed nothing again"
    );
    assert_eq!(first, second);
}

#[test]
fn a_file_with_no_edits_has_no_regions() {
    let log = Checkpointer::new();
    let history = log.history();
    assert!(history.regions("never-touched").is_empty());
    assert!(!history.has_edits("never-touched"));
}

// -- US-125: dependencies ----------------------------------------------------

#[test]
fn a_replacement_depends_on_the_producers_of_the_lines_it_replaces() {
    let mut log = Checkpointer::new();
    turn_from(&mut log, 1, text(""), text("one\ntwo\nthree\n"));
    turn(&mut log, 2, text("one\nTWO\nthree\n"));

    assert_eq!(
        summary(&log),
        vec![
            ((2, 0), vec![], Decision::Pending),
            ((4, 0), vec![(2, 0)], Decision::Pending),
        ]
    );
}

#[test]
fn an_insertion_depends_on_the_line_above_it_or_below_it_at_the_top() {
    let mut above = Checkpointer::new();
    turn_from(&mut above, 1, text("mid\nbot\n"), text("top\nmid\nbot\n"));
    turn(&mut above, 2, text("top\nmid\nbot\nend\n"));
    assert_eq!(
        summary(&above),
        vec![
            ((2, 0), vec![], Decision::Pending),
            ((4, 0), vec![], Decision::Pending),
        ],
        "the line above the insertion is an original line, so it has no producer"
    );

    let mut top = Checkpointer::new();
    turn_from(&mut top, 1, text("b\nc\n"), text("x\nb\nc\n"));
    turn(&mut top, 2, text("y\nx\nb\nc\n"));
    assert_eq!(
        summary(&top),
        vec![
            ((2, 0), vec![], Decision::Pending),
            ((4, 0), vec![(2, 0)], Decision::Pending),
        ],
        "an insertion at the top anchors on the line below it"
    );
}

#[test]
fn an_insertion_into_a_file_with_no_prior_edit_depends_on_nothing() {
    let mut log = Checkpointer::new();
    turn_from(&mut log, 1, text("b\nc\n"), text("a\nb\nc\n"));

    assert_eq!(summary(&log), vec![((2, 0), vec![], Decision::Pending)]);
}

#[test]
fn a_text_edit_over_an_applied_barrier_depends_on_it() {
    let mut log = Checkpointer::new();
    turn_from(&mut log, 1, text("a\nb\n"), FileState::absent());
    turn(&mut log, 2, text("a\nb\n"));
    turn(&mut log, 3, text("a\nB\n"));

    assert_eq!(
        summary(&log),
        vec![
            ((2, 0), vec![], Decision::Pending),
            ((4, 0), vec![(2, 0)], Decision::Pending),
            ((6, 0), vec![(2, 0), (4, 0)], Decision::Pending),
        ]
    );
}

#[test]
fn an_opaque_edit_depends_on_everything_applied() {
    let mut log = Checkpointer::new();
    turn_from(&mut log, 1, text("A\nB\n"), text("a\nb\n"));
    turn(&mut log, 2, FileState::absent());

    assert_eq!(
        summary(&log),
        vec![
            ((2, 0), vec![], Decision::Pending),
            ((4, 0), vec![(2, 0)], Decision::Pending),
        ]
    );
}

#[test]
fn lines_written_by_several_regions_are_each_depended_on_once() {
    let mut log = Checkpointer::new();
    turn_from(&mut log, 1, text("a\nb\nc\n"), text("A\nb\nC\n"));
    turn(&mut log, 2, text("Z\n"));

    assert_eq!(
        summary(&log),
        vec![
            ((2, 0), vec![], Decision::Pending),
            ((2, 1), vec![], Decision::Pending),
            ((4, 0), vec![(2, 0), (2, 1)], Decision::Pending),
        ]
    );
}

#[test]
fn dependencies_are_fixed_when_the_edit_is_appended() {
    let mut log = Checkpointer::new();
    turn_from(&mut log, 1, text(""), text("one\ntwo\n"));
    turn(&mut log, 2, text("one\nTWO\n"));
    let before_more_edits = summary(&log);

    turn(&mut log, 3, text("ONE\nTWO\n"));

    let after = summary(&log);
    assert_eq!(&after[..2], &before_more_edits[..]);
}

// -- US-126: effective decisions ---------------------------------------------

#[test]
fn a_revert_drags_everything_built_on_it() {
    let mut log = Checkpointer::new();
    turn_from(&mut log, 1, text(""), text("one\ntwo\n"));
    turn(&mut log, 2, text("one\nTWO\n"));
    let region_ids = ids(&log);

    log.decide_region(PATH, region_ids[0], Decision::Revert)
        .unwrap();

    assert_eq!(
        summary(&log),
        vec![
            ((2, 0), vec![], Decision::Revert),
            ((4, 0), vec![(2, 0)], Decision::Revert),
        ]
    );
}

#[test]
fn a_region_with_no_recorded_decision_is_pending_and_an_explicit_keep_is_kept() {
    let mut log = Checkpointer::new();
    turn_from(&mut log, 1, text(""), text("one\ntwo\n"));
    turn(&mut log, 2, text("one\nTWO\n"));
    let region_ids = ids(&log);
    assert_eq!(
        log.history().effective(PATH).get(&region_ids[0]),
        Some(&Decision::Pending)
    );

    log.decide_region(PATH, region_ids[1], Decision::Keep)
        .unwrap();

    assert_eq!(
        summary(&log),
        vec![
            ((2, 0), vec![], Decision::Keep),
            ((4, 0), vec![(2, 0)], Decision::Keep),
        ],
        "keeping a region keeps the pending region it was built on"
    );
    assert!(log.history().is_fully_reviewed(PATH));
}

#[test]
fn a_reverted_region_stays_reverted_when_a_keep_arrives_after_it() {
    let mut log = Checkpointer::new();
    turn_from(&mut log, 1, text(""), text("one\n"));
    let region_ids = ids(&log);

    log.decide_region(PATH, region_ids[0], Decision::Revert)
        .unwrap();
    log.decide_region(PATH, region_ids[0], Decision::Keep)
        .unwrap();

    assert_eq!(summary(&log), vec![((2, 0), vec![], Decision::Revert)]);
}

// -- US-127: reconstruction --------------------------------------------------

#[test]
fn a_projection_with_nothing_reverted_is_the_last_after_state() {
    let mut log = Checkpointer::new();
    turn_from(
        &mut log,
        1,
        text("a\nb\nc\nd\ne\n"),
        text("A\nb\nC\nd\nE\n"),
    );

    let history = log.history();
    assert_eq!(history.content(PATH), text("A\nb\nC\nd\nE\n"));
    assert_eq!(
        history.accepted_baseline(PATH),
        text("a\nb\nc\nd\ne\n"),
        "nothing has been kept, so the accepted baseline is the original"
    );
}

#[test]
fn the_accepted_baseline_holds_exactly_the_kept_regions() {
    let mut log = Checkpointer::new();
    turn_from(
        &mut log,
        1,
        text("a\nb\nc\nd\ne\n"),
        text("A\nb\nC\nd\nE\n"),
    );
    let region_ids = ids(&log);

    log.decide_region(PATH, region_ids[1], Decision::Keep)
        .unwrap();

    let history = log.history();
    assert_eq!(history.accepted_baseline(PATH), text("a\nb\nC\nd\ne\n"));
    assert_eq!(history.content(PATH), text("A\nb\nC\nd\nE\n"));
}

#[test]
fn spans_are_rebased_through_each_edits_own_before_state() {
    let mut log = Checkpointer::new();
    turn_from(
        &mut log,
        1,
        text("a\nb\nc\nd\ne\n"),
        text("a\nb\nc\nd\ne\n"),
    );
    turn(&mut log, 2, text("a\nB\nc\nD\ne\n"));
    turn(&mut log, 3, text("a\nB\nC\nD\ne\n"));
    assert_eq!(
        summary(&log),
        vec![
            ((3, 0), vec![], Decision::Pending),
            ((3, 1), vec![], Decision::Pending),
            ((5, 0), vec![], Decision::Pending),
        ],
        "the first turn changed nothing, so it appended no edit"
    );
    assert_eq!(log.history().content(PATH), text("a\nB\nC\nD\ne\n"));

    let region_ids = ids(&log);
    log.decide_region(PATH, region_ids[0], Decision::Revert)
        .unwrap();

    assert_eq!(
        log.history().content(PATH),
        text("a\nb\nC\nD\ne\n"),
        "the later turn's hunk still lands on its own line"
    );
}

#[test]
fn an_applied_whole_file_unit_replaces_the_file_and_later_edits_splice_onto_it() {
    let mut log = Checkpointer::new();
    turn_from(&mut log, 1, text("a\nb\n"), FileState::absent());
    turn(&mut log, 2, text("a\nb\nc\n"));

    let history = log.history();
    assert_eq!(history.content(PATH), text("a\nb\nc\n"));
    assert_eq!(history.accepted_baseline(PATH), text("a\nb\n"));
}

#[test]
fn a_region_whose_dependency_is_not_applied_is_left_out_of_every_projection() {
    let mut log = Checkpointer::new();
    turn_from(&mut log, 1, text(""), text("one\ntwo\n"));
    turn(&mut log, 2, text("one\nTWO\n"));
    let region_ids = ids(&log);

    log.decide_region(PATH, region_ids[0], Decision::Revert)
        .unwrap();

    let history = log.history();
    assert_eq!(history.content(PATH), text(""));
    assert_eq!(history.accepted_baseline(PATH), text(""));
}

#[test]
fn a_partial_revert_re_encodes_the_way_the_file_was_found() {
    // Latin-1 bytes with CRLF endings: the codec cannot be UTF-8 and the line
    // ending cannot be LF, so a reconstruction that rewrote either would show.
    let original =
        FileState::from_bytes(b"caf\xe9\r\nsecond\r\nthird\r\nfourth\r\nfifth\r\n".to_vec());
    let rewritten =
        FileState::from_bytes(b"caf\xe9\r\nSECOND\r\nthird\r\nFOURTH\r\nfifth\r\n".to_vec());
    let mut log = Checkpointer::new();
    turn_from(&mut log, 1, original, rewritten);
    let region_ids = ids(&log);

    log.decide_region(PATH, region_ids[0], Decision::Revert)
        .unwrap();

    let projected = log.history().content(PATH);
    assert_eq!(
        projected.data().map(<[u8]>::to_vec),
        Some(b"caf\xe9\r\nsecond\r\nthird\r\nFOURTH\r\nfifth\r\n".to_vec()),
        "the line ending and the single-byte codec survive the partial revert"
    );
}

#[test]
fn a_span_that_rebases_backwards_lands_where_the_reference_lands_it() {
    // Keeping a region whose lines the projection carries from the original file
    // selects a baseline that holds that region alone. Its span then rebases
    // through a diff that moved both of its ends into different blocks and comes
    // back inverted, which the reference resolves as an insertion at the start.
    // The expectations are what the pinned reference answered for this log.
    let mut log = Checkpointer::new();
    turn_from(
        &mut log,
        1,
        text("x\nd\na\n\ny\ndup\n"),
        text("\nx\n\ndup\nd\n\nc\n"),
    );
    turn(&mut log, 2, text("x\ndup\nc\n"));
    let region_ids = ids(&log);

    log.decide_region(PATH, region_ids[6], Decision::Keep)
        .unwrap();

    let history = log.history();
    assert_eq!(history.content(PATH), text("x\ndup\nc\n"));
    assert_eq!(
        history.accepted_baseline(PATH),
        text("x\nd\na\n\ny\ndup\n"),
        "the kept region deletes lines the baseline never carried, so it lands as an \
         empty insertion rather than failing on an inverted span"
    );
    assert_eq!(
        anchors(&log, None),
        vec![
            (HunkSide::Deletions, 4, vec![(2, 0), (2, 2), (4, 0)]),
            (HunkSide::Additions, 2, vec![(2, 3)]),
        ]
    );
}

#[test]
fn a_span_that_rebases_past_the_end_lands_where_the_reference_lands_it() {
    // The other half of the same resolution: once a revert has dropped part of
    // what a later hunk was written against, that hunk's rebased span can reach
    // past what is left of the projection. Anchoring is what reconstructs the
    // provenance, so this used to be reached only through `pending_hunks`. The
    // expectations are what the pinned reference answered for this log.
    let mut log = Checkpointer::new();
    turn_from(&mut log, 1, text("a\n"), text("b\na\ndup"));
    turn(&mut log, 2, text("y\nb\nb\n"));
    turn(&mut log, 3, text(""));
    turn(&mut log, 4, text("y\nx\n"));
    turn(&mut log, 5, text("\na\ny\n"));
    let region_ids = ids(&log);
    log.decide_region(PATH, region_ids[3], Decision::Revert)
        .unwrap();
    turn(&mut log, 6, text("b\n"));

    let history = log.history();
    assert_eq!(history.content(PATH), text("b\n"));
    assert_eq!(history.accepted_baseline(PATH), text("a\n"));
    assert!(
        history.pending_hunks(PATH, None).is_empty(),
        "every rendered block attributes to a region the baseline already carries"
    );
}

#[test]
fn a_file_with_no_edits_projects_its_original_state() {
    let mut log = Checkpointer::new();
    log.begin_turn(1).unwrap();
    log.record_pre_edit(PATH, text("untouched\n")).unwrap();
    log.seal_turn();

    let history = log.history();
    assert_eq!(history.content(PATH), text("untouched\n"));
    assert_eq!(history.original(PATH), text("untouched\n"));
}

#[test]
fn every_region_reverted_projects_the_original_byte_for_byte() {
    let original = FileState::from_bytes(b"a\r\nb\r\nc\r\n".to_vec());
    let mut log = Checkpointer::new();
    turn_from(&mut log, 1, original.clone(), text("A\nb\nC\n"));
    log.decide_file(PATH, Decision::Revert).unwrap();

    assert_eq!(log.history().content(PATH), original);
}

// -- US-128: hunk anchors ----------------------------------------------------

#[test]
fn an_addition_anchors_on_the_current_side_at_the_blocks_last_line() {
    let mut log = Checkpointer::new();
    turn_from(&mut log, 1, text("a\nb\n"), text("a\nNEW\nb\n"));

    assert_eq!(
        anchors(&log, None),
        vec![(HunkSide::Additions, 1, vec![(2, 0)])]
    );
}

#[test]
fn a_pure_deletion_anchors_on_the_baseline_side() {
    let mut log = Checkpointer::new();
    turn_from(&mut log, 1, text("a\nb\nc\n"), text("a\nc\n"));

    assert_eq!(
        anchors(&log, None),
        vec![(HunkSide::Deletions, 1, vec![(2, 0)])]
    );
}

#[test]
fn two_identical_deletions_are_claimed_by_distinct_runs() {
    let mut log = Checkpointer::new();
    turn_from(
        &mut log,
        1,
        text("keep\ndup\nmid\nkeep\ndup\n"),
        text("keep\nmid\nkeep\n"),
    );

    assert_eq!(
        anchors(&log, None),
        vec![
            (HunkSide::Deletions, 1, vec![(2, 0)]),
            (HunkSide::Deletions, 4, vec![(2, 1)]),
        ],
        "neither deletion is decided by the other's control"
    );
}

#[test]
fn an_anchor_targets_the_whole_dependency_component() {
    let mut log = Checkpointer::new();
    turn_from(&mut log, 1, text(""), text("one\ntwo\n"));
    turn(&mut log, 2, text("one\nTWO\n"));

    assert_eq!(
        anchors(&log, None),
        vec![(HunkSide::Additions, 1, vec![(2, 0), (4, 0)])]
    );
}

#[test]
fn a_scope_anchors_against_that_owners_own_change() {
    let mut log = Checkpointer::new();
    turn_from(&mut log, 1, text(""), text("one\ntwo\n"));
    turn(&mut log, 2, text("one\nTWO\n"));

    assert_eq!(
        anchors(&log, Some(agent(1))),
        vec![(HunkSide::Additions, 1, vec![(2, 0)])]
    );
    assert_eq!(
        anchors(&log, Some(agent(2))),
        vec![(HunkSide::Additions, 1, vec![(4, 0)])]
    );
    assert_eq!(
        log.history().scope_pending_diff(PATH, agent(2)),
        Some((text("one\ntwo\n"), text("one\nTWO\n"))),
    );
}

#[test]
fn a_scope_with_nothing_pending_anchors_nothing() {
    let mut log = Checkpointer::new();
    turn_from(&mut log, 1, text(""), text("one\ntwo\n"));
    let region_ids = ids(&log);
    log.decide_region(PATH, region_ids[0], Decision::Keep)
        .unwrap();

    assert!(anchors(&log, Some(agent(1))).is_empty());
    assert!(anchors(&log, None).is_empty());
    assert_eq!(log.history().scope_pending_diff(PATH, agent(1)), None);
    assert_eq!(
        log.history()
            .scope_pending_diff(PATH, Owner::Manual { index: 9 }),
        None,
        "an owner that never touched the path has no scope diff"
    );
}

#[test]
fn a_diff_block_attributing_to_no_pending_region_anchors_nothing() {
    // A deletion of the whole file is a whole-file unit, so the delete block it
    // renders belongs to no line hunk and carries no control.
    let mut log = Checkpointer::new();
    turn_from(&mut log, 1, text("a\nb\n"), FileState::absent());

    assert!(anchors(&log, None).is_empty());
}

#[test]
fn a_file_with_no_edits_anchors_nothing() {
    let log = Checkpointer::new();
    assert!(
        log.history()
            .pending_hunks("never-touched", None)
            .is_empty()
    );
    assert!(
        log.history()
            .pending_hunks("never-touched", Some(agent(1)))
            .is_empty()
    );
}

#[test]
fn a_decided_region_leaves_only_the_pending_one_anchored() {
    let mut kept = Checkpointer::new();
    turn_from(&mut kept, 1, text("a\nb\nc\n"), text("A\nb\nC\n"));
    let region_ids = ids(&kept);
    kept.decide_region(PATH, region_ids[0], Decision::Keep)
        .unwrap();
    assert_eq!(
        anchors(&kept, None),
        vec![(HunkSide::Additions, 2, vec![(2, 1)])]
    );

    let mut reverted = Checkpointer::new();
    turn_from(&mut reverted, 1, text("a\nb\nc\n"), text("A\nb\nC\n"));
    let region_ids = ids(&reverted);
    reverted
        .decide_region(PATH, region_ids[0], Decision::Revert)
        .unwrap();
    assert_eq!(
        anchors(&reverted, None),
        vec![(HunkSide::Additions, 2, vec![(2, 1)])]
    );
    assert_eq!(reverted.history().content(PATH), text("a\nb\nC\n"));
}

// -- US-130: what a truncation would restore ---------------------------------

/// A restore plan flattened to paths and their target text, so an expectation
/// reads in the order the plan names them.
fn plan_summary(plan: &[(String, FileState)]) -> Vec<(String, Option<String>)> {
    plan.iter()
        .map(|(path, state)| {
            (
                path.clone(),
                state
                    .data()
                    .map(|bytes| String::from_utf8_lossy(bytes).into_owned()),
            )
        })
        .collect()
}

/// One turn writing `after` to `path`, starting from what the log projects.
fn turn_on(log: &mut Checkpointer, turn_id: u64, path: &str, after: FileState) {
    let before = log.history().content(path);
    log.begin_turn(turn_id).unwrap();
    log.record_pre_edit(path, before).unwrap();
    log.record_post_edit(path, after).unwrap();
    log.seal_turn();
}

#[test]
fn the_log_names_every_path_it_touched_in_the_order_it_first_touched_it() {
    let mut log = Checkpointer::new();
    turn_on(&mut log, 1, "b", text("b\n"));
    turn_on(&mut log, 2, "a", text("a\n"));
    // A path a turn recorded but never changed is still tracked: the recording
    // is what a later drift is measured against.
    log.begin_turn(3).unwrap();
    log.record_pre_edit("c", text("c\n")).unwrap();
    log.seal_turn();

    assert_eq!(log.history().tracked_paths(), vec!["b", "a", "c"]);
    assert_eq!(log.history().last_turn_paths(), vec!["c"]);
    assert!(Checkpointer::new().history().last_turn_paths().is_empty());
}

#[test]
fn a_turn_resolves_to_its_own_mark_and_an_absent_one_to_the_next() {
    let mut log = Checkpointer::new();
    turn_from(&mut log, 2, text("a\n"), text("b\n"));
    turn_from(&mut log, 5, text("b\n"), text("c\n"));
    let history = log.history();

    assert!(history.has_turn(2));
    assert!(!history.has_turn(3));
    assert_eq!(history.event_index_of_turn(2), Some(0));
    assert_eq!(
        history.event_index_of_turn(3),
        history.event_index_of_turn(5),
        "a turn the log never carried cuts where the next one begins"
    );
    assert_eq!(history.event_index_of_turn(9), None);
}

#[test]
fn a_reused_turn_identifier_resolves_to_the_newest_mark_carrying_it() {
    let mut log = Checkpointer::new();
    turn_from(&mut log, 1, text("a\n"), text("b\n"));
    // A transcript reset can hand the same identifier back out.
    turn_from(&mut log, 1, text("b\n"), text("c\n"));

    let index = log
        .history()
        .event_index_of_turn(1)
        .expect("both marks carry the identifier");

    assert_eq!(
        plan_summary(&log.history().restore_plan(index)),
        vec![(PATH.to_owned(), Some("b\n".to_owned()))],
        "the newest mark wins, so the cut restores what the second turn found"
    );
}

#[test]
fn a_dropped_turns_files_restore_to_what_that_turn_found() {
    let mut log = Checkpointer::new();
    turn_on(&mut log, 1, "kept", text("kept once\n"));
    turn_on(&mut log, 2, "rolled", text("rolled once\n"));
    turn_on(&mut log, 3, "rolled", text("rolled twice\n"));

    let plan = log.history().restore_plan_to_turn(2);

    assert_eq!(
        plan_summary(&plan),
        vec![("rolled".to_owned(), None)],
        "the earliest dropped turn's recording wins, and it found no file there"
    );
    assert!(
        log.history().restore_plan_to_turn(9).is_empty(),
        "a turn the log does not carry restores nothing"
    );
}

#[test]
fn a_file_only_a_dropped_hand_edit_touched_restores_to_the_kept_projection() {
    let mut log = Checkpointer::new();
    turn_on(&mut log, 1, PATH, text("agent\n"));
    turn_on(&mut log, 2, "other", text("other\n"));
    log.reconcile(PATH, text("agent\nby hand\n"));

    // Cutting at turn 2 drops the turn's own path and the later hand edit.
    let plan = log.history().restore_plan_to_turn(2);

    assert_eq!(
        plan_summary(&plan),
        vec![
            ("other".to_owned(), None),
            (PATH.to_owned(), Some("agent\n".to_owned())),
        ],
        "the hand edit left no recording, so its path restores to what the kept log projects"
    );
}

#[test]
fn a_decision_taken_after_the_cut_does_not_shape_the_plan() {
    let mut log = Checkpointer::new();
    turn_on(&mut log, 1, PATH, text("one\n"));
    turn_on(&mut log, 2, PATH, text("one\ntwo\n"));
    let regions = ids(&log);
    log.decide_region(PATH, regions[1], Decision::Revert)
        .unwrap();

    let plan = log.history().restore_plan_to_turn(2);

    assert_eq!(
        plan_summary(&plan),
        vec![(PATH.to_owned(), Some("one\n".to_owned()))],
        "the plan is the recording, not the projection the revert already produced"
    );
}
