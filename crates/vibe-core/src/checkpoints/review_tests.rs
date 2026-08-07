//! What the review projection publishes, and what the seven targets decide.
//!
//! The engine's own answers are measured against the pinned reference by
//! `checkpoint_parity_tests`. What is left to prove here is the narrowing on top
//! of them: which files reach a panel, what each region carries once its lines
//! are dropped, which slot each owner keeps, and which regions each of the seven
//! granularities reaches. Every scenario is built through the log's own
//! lifecycle rather than by planting events.

use std::collections::BTreeMap;

use super::checkpointer::Checkpointer;
use super::lines::FileState;
use super::models::{Decision, OpaqueReason, Owner, RegionId};
use super::review::{
    ReviewFileStatus, ReviewRegion, ReviewTarget, decide_target, project_scope_diff, project_state,
};

const PATH: &str = "f";

fn text(value: &str) -> FileState {
    FileState::from_text(value)
}

fn agent(turn_id: u64) -> Owner {
    Owner::Agent { turn_id }
}

/// One turn rewriting `path` from what the log projects to `after`.
fn turn_on(log: &mut Checkpointer, turn_id: u64, path: &str, after: FileState) {
    let before = log.history().content(path);
    log.begin_turn(turn_id).unwrap();
    log.record_pre_edit(path, before).unwrap();
    log.record_post_edit(path, after).unwrap();
    log.seal_turn();
}

fn turn(log: &mut Checkpointer, turn_id: u64, after: FileState) {
    turn_on(log, turn_id, PATH, after);
}

/// The states a caller would have read from disk for every tracked path,
/// assuming disk holds what the log projects.
fn on_disk(log: &Checkpointer) -> BTreeMap<String, FileState> {
    log.history()
        .tracked_paths()
        .into_iter()
        .map(|path| {
            let state = log.history().content(&path);
            (path, state)
        })
        .collect()
}

#[test]
fn a_text_region_projects_its_identity_owner_spans_and_dependencies() {
    let mut log = Checkpointer::new();
    turn(&mut log, 1, text("one\ntwo\n"));
    turn(&mut log, 2, text("one\ntwo\nthree\n"));

    let state = project_state(&log.history(), &on_disk(&log));
    let file = state.files.first().expect("one changed file");
    assert_eq!(file.path, PATH);
    assert_eq!(file.status, ReviewFileStatus::Created);

    let ReviewRegion::Text(second) = file.regions.get(1).expect("the second turn's region") else {
        panic!("a line change projects as text");
    };
    assert_eq!(second.owner, agent(2));
    assert_eq!(second.decision, Decision::Pending);
    assert_eq!(second.baseline_line_count, 0);
    assert_eq!(second.current_start, 2);
    assert_eq!(second.current_line_count, 1);
    assert_eq!(
        second.depends_on,
        vec![RegionId::new(2, 0)],
        "the appended line sits under the line the first turn wrote"
    );
}

#[test]
fn an_opaque_region_projects_its_reason_and_carries_no_line_coordinates() {
    let mut log = Checkpointer::new();
    turn(&mut log, 1, text("readable\n"));
    turn(&mut log, 2, FileState::from_bytes(b"\0\x01binary".to_vec()));

    let state = project_state(&log.history(), &on_disk(&log));
    let file = state.files.first().expect("one changed file");
    assert_eq!(file.status, ReviewFileStatus::BinaryOrUndecodable);
    let ReviewRegion::Opaque(opaque) = file.regions.get(1).expect("the binary rewrite") else {
        panic!("a binary side has no lines to diff");
    };
    assert_eq!(opaque.reason, OpaqueReason::BinaryOrUndecodable);
    assert_eq!(opaque.ordinal, 0);
    assert_eq!(opaque.owner, agent(2));
}

#[test]
fn a_file_whose_regions_are_all_decided_leaves_the_state() {
    let mut log = Checkpointer::new();
    turn(&mut log, 1, text("one\n"));

    let state = project_state(&log.history(), &on_disk(&log));
    assert_eq!(state.files.len(), 1, "one pending change is reviewable");

    log.decide_file(PATH, Decision::Keep).unwrap();
    let resolved = project_state(&log.history(), &on_disk(&log));
    assert!(
        resolved.files.is_empty(),
        "a file with nothing pending is resolved, not empty-listed"
    );
    assert_eq!(
        resolved.scopes.len(),
        1,
        "the owner keeps its slot after its file leaves"
    );
    assert!(
        resolved.scopes[0].files.is_empty(),
        "a fully decided slot carries no files"
    );
}

#[test]
fn a_files_status_names_deletion_before_opacity_and_creation_before_modification() {
    let mut existing = Checkpointer::new();
    existing.begin_turn(1).unwrap();
    existing.record_pre_edit(PATH, text("before\n")).unwrap();
    existing.record_post_edit(PATH, text("after\n")).unwrap();
    existing.seal_turn();
    let modified = project_state(&existing.history(), &on_disk(&existing));
    assert_eq!(modified.files[0].status, ReviewFileStatus::Modified);

    let mut created = Checkpointer::new();
    turn(&mut created, 1, text("new\n"));
    let created_state = project_state(&created.history(), &on_disk(&created));
    assert_eq!(created_state.files[0].status, ReviewFileStatus::Created);

    let mut deleted = Checkpointer::new();
    deleted.begin_turn(1).unwrap();
    deleted.record_pre_edit(PATH, text("gone\n")).unwrap();
    deleted.record_post_edit(PATH, FileState::absent()).unwrap();
    deleted.seal_turn();
    let deleted_state = project_state(&deleted.history(), &on_disk(&deleted));
    assert_eq!(
        deleted_state.files[0].status,
        ReviewFileStatus::Deleted,
        "a file that is gone is deleted whatever its regions look like"
    );
}

#[test]
fn every_owner_keeps_a_slot_in_log_order_with_its_still_pending_files() {
    let mut log = Checkpointer::new();
    turn(&mut log, 1, text("one\n"));
    log.reconcile(PATH, text("one\nby hand\n"))
        .expect("room for the drift");
    turn(&mut log, 2, text("one\nby hand\ntwo\n"));

    let state = project_state(&log.history(), &on_disk(&log));
    let owners: Vec<Owner> = state.scopes.iter().map(|scope| scope.owner).collect();
    assert_eq!(
        owners,
        vec![agent(1), Owner::Manual { index: 1 }, agent(2)],
        "the hand edit keeps its own slot between the two turns"
    );
    for scope in &state.scopes {
        assert_eq!(scope.files.len(), 1, "each slot changed the one file");
        assert_eq!(scope.files[0].path, PATH);
        assert_eq!(scope.files[0].region_count, 1);
        assert_eq!(scope.files[0].status, ReviewFileStatus::Created);
    }
}

#[test]
fn a_log_with_no_tracked_file_projects_empty_files_and_scopes() {
    let log = Checkpointer::new();
    let state = project_state(&log.history(), &BTreeMap::new());
    assert!(state.files.is_empty());
    assert!(state.scopes.is_empty());
}

#[test]
fn a_scope_diff_for_an_owner_that_did_not_touch_the_path_is_modified_and_empty() {
    let mut log = Checkpointer::new();
    turn(&mut log, 1, text("one\n"));

    let diff = project_scope_diff(&log.history(), PATH, agent(9));
    assert_eq!(diff.status, ReviewFileStatus::Modified);
    assert!(diff.baseline.is_empty());
    assert!(diff.current.is_empty());

    let own = project_scope_diff(&log.history(), PATH, agent(1));
    assert_eq!(own.status, ReviewFileStatus::Created);
    assert_eq!(own.current, "one\n");
}

#[test]
fn a_scope_diff_reports_deletion_and_opacity_rather_than_rendering_them() {
    let mut deleted = Checkpointer::new();
    deleted.begin_turn(1).unwrap();
    deleted.record_pre_edit(PATH, text("gone\n")).unwrap();
    deleted.record_post_edit(PATH, FileState::absent()).unwrap();
    deleted.seal_turn();
    let removal = project_scope_diff(&deleted.history(), PATH, agent(1));
    assert_eq!(removal.status, ReviewFileStatus::Deleted);
    assert_eq!(removal.baseline, "gone\n");
    assert!(removal.current.is_empty());

    let mut binary = Checkpointer::new();
    binary.begin_turn(1).unwrap();
    binary.record_pre_edit(PATH, text("readable\n")).unwrap();
    binary
        .record_post_edit(PATH, FileState::from_bytes(b"\0\x01".to_vec()))
        .unwrap();
    binary.seal_turn();
    let rewrite = project_scope_diff(&binary.history(), PATH, agent(1));
    assert_eq!(rewrite.status, ReviewFileStatus::BinaryOrUndecodable);
    assert!(
        rewrite.baseline.is_empty() && rewrite.current.is_empty(),
        "a side with no lines renders as nothing rather than as bytes"
    );
}

/// Each of the seven granularities, against the same three-turn log, checking
/// only that it reached what it names and left the rest pending.
#[test]
fn the_seven_targets_reach_exactly_what_they_name() {
    let build = || {
        let mut log = Checkpointer::new();
        turn_on(&mut log, 1, "a", text("one\n"));
        turn_on(&mut log, 2, "a", text("one\ntwo\n"));
        turn_on(&mut log, 3, "b", text("elsewhere\n"));
        log
    };
    let kept = |log: &Checkpointer, path: &str| {
        log.history()
            .regions(path)
            .into_iter()
            .map(|region| region.decision)
            .collect::<Vec<_>>()
    };

    let mut region = build();
    let paths = decide_target(
        &mut region,
        &ReviewTarget::Region {
            path: "a".to_owned(),
            version_index: 2,
            ordinal: 0,
        },
        Decision::Keep,
    )
    .unwrap();
    assert_eq!(paths, vec!["a"]);
    assert_eq!(kept(&region, "a"), vec![Decision::Keep, Decision::Pending]);

    let mut regions = build();
    let identifiers: Vec<RegionId> = regions
        .history()
        .regions("a")
        .into_iter()
        .map(|region| region.region_id)
        .collect();
    decide_target(
        &mut regions,
        &ReviewTarget::Regions {
            path: "a".to_owned(),
            regions: identifiers,
        },
        Decision::Keep,
    )
    .unwrap();
    assert_eq!(kept(&regions, "a"), vec![Decision::Keep, Decision::Keep]);
    assert_eq!(kept(&regions, "b"), vec![Decision::Pending]);

    let mut file = build();
    decide_target(
        &mut file,
        &ReviewTarget::File {
            path: "a".to_owned(),
        },
        Decision::Keep,
    )
    .unwrap();
    assert_eq!(kept(&file, "a"), vec![Decision::Keep, Decision::Keep]);
    assert_eq!(kept(&file, "b"), vec![Decision::Pending]);

    let mut scope_file = build();
    decide_target(
        &mut scope_file,
        &ReviewTarget::ScopeFile {
            owner: agent(1),
            path: "a".to_owned(),
        },
        Decision::Keep,
    )
    .unwrap();
    assert_eq!(
        kept(&scope_file, "a"),
        vec![Decision::Keep, Decision::Pending]
    );

    let mut scope = build();
    turn_on(&mut scope, 4, "b", text("elsewhere\nagain\n"));
    let touched = decide_target(
        &mut scope,
        &ReviewTarget::Scope { owner: agent(3) },
        Decision::Keep,
    )
    .unwrap();
    assert_eq!(touched, vec!["b"], "a scope reports the paths it reached");

    let mut all = build();
    let everywhere = decide_target(&mut all, &ReviewTarget::All, Decision::Keep).unwrap();
    assert_eq!(everywhere, vec!["a", "b"]);
    assert_eq!(kept(&all, "a"), vec![Decision::Keep, Decision::Keep]);
    assert_eq!(kept(&all, "b"), vec![Decision::Keep]);

    let mut none = build();
    let nothing = decide_target(
        &mut none,
        &ReviewTarget::LastTurns { count: 0 },
        Decision::Keep,
    )
    .unwrap();
    assert!(nothing.is_empty(), "a count of zero decides nothing");
    let negative = decide_target(
        &mut none,
        &ReviewTarget::LastTurns { count: -3 },
        Decision::Keep,
    )
    .unwrap();
    assert!(negative.is_empty(), "a negative count decides nothing");
}

#[test]
fn the_last_turns_target_counts_back_from_the_accepted_frontier() {
    let mut log = Checkpointer::new();
    turn_on(&mut log, 1, "a", text("one\n"));
    turn_on(&mut log, 2, "a", text("one\ntwo\n"));
    turn_on(&mut log, 3, "a", text("one\ntwo\nthree\n"));
    assert_eq!(log.history().turn_ids(), vec![1, 2, 3]);
    assert_eq!(
        log.history().accepted_turn_frontier(),
        0,
        "nothing is accepted yet, so the frontier is at the start"
    );

    decide_target(
        &mut log,
        &ReviewTarget::LastTurns { count: 1 },
        Decision::Keep,
    )
    .unwrap();
    let decisions: Vec<Decision> = log
        .history()
        .regions("a")
        .into_iter()
        .map(|region| region.decision)
        .collect();
    assert_eq!(
        decisions,
        vec![Decision::Keep, Decision::Keep, Decision::Keep],
        "keeping the newest turn pulls in the earlier hunks it was built on"
    );
    assert_eq!(
        log.history().accepted_turn_frontier(),
        3,
        "every turn is fully kept, so the frontier moved past all of them"
    );
    assert!(
        decide_target(
            &mut log,
            &ReviewTarget::LastTurns { count: 2 },
            Decision::Revert,
        )
        .unwrap()
        .is_empty(),
        "nothing sits past the frontier, so there is no turn left to decide"
    );
}

#[test]
fn a_target_naming_a_region_the_file_does_not_carry_is_refused() {
    let mut log = Checkpointer::new();
    turn(&mut log, 1, text("one\n"));
    let before = log.history().regions(PATH);

    let error = decide_target(
        &mut log,
        &ReviewTarget::Region {
            path: PATH.to_owned(),
            version_index: 99,
            ordinal: 4,
        },
        Decision::Keep,
    )
    .expect_err("a region the file does not carry");
    assert!(error.to_string().contains("99.4"), "{error}");
    assert_eq!(log.history().regions(PATH), before, "the log is unchanged");
}

#[test]
fn a_decision_is_refused_while_a_turn_is_open() {
    let mut log = Checkpointer::new();
    turn(&mut log, 1, text("one\n"));
    log.begin_turn(2).unwrap();

    let error = decide_target(&mut log, &ReviewTarget::All, Decision::Revert)
        .expect_err("a decision during a turn");
    assert!(error.to_string().contains("turn is open"), "{error}");
}

/// The wire spelling is the contract a client sends back, so the target
/// vocabulary is deserialized from the exact shapes the app-server census
/// records rather than from whatever serde would accept.
#[test]
fn the_seven_targets_deserialize_from_the_wire_shapes_the_census_records() {
    let cases = [
        (
            serde_json::json!({"kind": "region", "path": "a", "versionIndex": 2, "ordinal": 1}),
            ReviewTarget::Region {
                path: "a".to_owned(),
                version_index: 2,
                ordinal: 1,
            },
        ),
        (
            serde_json::json!({
                "kind": "regions",
                "path": "a",
                "regions": [{"versionIndex": 2, "ordinal": 0}]
            }),
            ReviewTarget::Regions {
                path: "a".to_owned(),
                regions: vec![RegionId::new(2, 0)],
            },
        ),
        (
            serde_json::json!({"kind": "scope", "owner": {"kind": "agent", "turnId": 7}}),
            ReviewTarget::Scope { owner: agent(7) },
        ),
        (
            serde_json::json!({
                "kind": "scopeFile",
                "owner": {"kind": "manual", "index": 2},
                "path": "a"
            }),
            ReviewTarget::ScopeFile {
                owner: Owner::Manual { index: 2 },
                path: "a".to_owned(),
            },
        ),
        (
            serde_json::json!({"kind": "file", "path": "a"}),
            ReviewTarget::File {
                path: "a".to_owned(),
            },
        ),
        (serde_json::json!({"kind": "all"}), ReviewTarget::All),
        (
            serde_json::json!({"kind": "lastTurns", "count": 3}),
            ReviewTarget::LastTurns { count: 3 },
        ),
    ];
    for (wire, expected) in cases {
        let parsed: ReviewTarget =
            serde_json::from_value(wire.clone()).unwrap_or_else(|error| panic!("{wire}: {error}"));
        assert_eq!(parsed, expected, "{wire}");
    }
}

/// The region projection is what a client addresses a decision with, so its
/// field names travel exactly as the census records them.
#[test]
fn a_projected_region_serializes_under_the_names_the_census_records() {
    let mut log = Checkpointer::new();
    turn(&mut log, 1, text("one\n"));
    turn(&mut log, 2, text("one\ntwo\n"));
    let state = project_state(&log.history(), &on_disk(&log));

    let encoded = serde_json::to_value(&state).expect("the projection encodes");
    let region = &encoded["files"][0]["regions"][1];
    assert_eq!(region["kind"], "text");
    assert_eq!(
        region["versionIndex"], 4,
        "the identity is the producing edit's sequence number, which counts marks too"
    );
    assert_eq!(region["ordinal"], 0);
    assert_eq!(
        region["owner"],
        serde_json::json!({"kind": "agent", "turnId": 2})
    );
    assert_eq!(region["baselineStart"], 1);
    assert_eq!(region["baselineLineCount"], 0);
    assert_eq!(region["currentStart"], 1);
    assert_eq!(region["currentLineCount"], 1);
    assert_eq!(region["decision"], "pending");
    assert_eq!(
        region["dependsOn"],
        serde_json::json!([{"versionIndex": 2, "ordinal": 0}])
    );
    assert_eq!(encoded["scopes"][0]["files"][0]["regionCount"], 1);
}

/// A hand edit is a first-class owner on the wire too, and an opaque change
/// publishes its reason where a text one publishes its spans.
#[test]
fn a_manual_owner_and_an_opaque_region_serialize_as_the_census_declares_them() {
    let mut log = Checkpointer::new();
    turn(&mut log, 1, text("one\n"));
    log.reconcile(PATH, FileState::from_bytes(b"\0by hand".to_vec()))
        .expect("room for the drift");
    let state = project_state(&log.history(), &on_disk(&log));

    let encoded = serde_json::to_value(&state).expect("the projection encodes");
    let region = &encoded["files"][0]["regions"][1];
    assert_eq!(region["kind"], "opaque");
    assert_eq!(region["reason"], "binary_or_undecodable");
    assert!(
        region.get("baselineStart").is_none(),
        "an opaque region carries no line coordinates: {region}"
    );
    assert_eq!(
        region["owner"],
        serde_json::json!({"kind": "manual", "index": 1})
    );
    assert_eq!(encoded["files"][0]["status"], "binary_or_undecodable");
    assert_eq!(
        encoded["scopes"][1]["owner"],
        serde_json::json!({"kind": "manual", "index": 1})
    );
}
