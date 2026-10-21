use tempfile::tempdir;

use super::*;
use crate::checkpoints::{HunkSide, RETAINED_BYTES_LIMIT, ReviewFileStatus};

/// A log is never shortened behind the caller it answers. The port used to
/// drop the oldest checkpoint once 64 had accumulated, which silently
/// retired identities a client was still holding; the ceiling replaced it,
/// and the ceiling refuses rather than trims.
#[test]
fn the_log_is_never_shortened_behind_the_caller() {
    let root = tempdir().expect("workspace");
    std::fs::write(root.path().join("bounded.txt"), "0").expect("seed file");
    let review = ReviewManager::new(Arc::new(Workspace::open(root.path()).expect("open")));

    let turns = 80;
    for turn in 1..=turns {
        review.begin_turn_at(turn).expect("begin turn");
        review
            .edit(
                "bounded.txt",
                &[EditOperation {
                    old_text: (turn - 1).to_string(),
                    new_text: turn.to_string(),
                    replace_all: false,
                }],
            )
            .expect("edit");
        review.seal_turn().expect("seal turn");
    }

    let (retained, exhausted) = review.with_log(|log| {
        (
            (1..=turns)
                .filter(|turn| log.history().has_turn(u64::try_from(*turn).unwrap_or(0)))
                .count(),
            log.retained_bytes(),
        )
    });
    assert_eq!(retained, turns, "every turn is still addressable");
    assert!(
        exhausted < RETAINED_BYTES_LIMIT,
        "this fixture is nowhere near the ceiling"
    );
    assert!(
        review.take_retention_notice().is_none(),
        "nothing was refused, so nothing is published"
    );
}

/// Reaching the ceiling refuses the capture and says so once. The write
/// itself still lands: the file changes, only its history stops being
/// tracked, and the operator is told rather than left to discover it.
#[test]
fn a_full_log_refuses_the_capture_and_publishes_it() {
    let root = tempdir().expect("workspace");
    std::fs::write(root.path().join("full.txt"), "one\n").expect("seed file");
    let review = ReviewManager::new(Arc::new(Workspace::open(root.path()).expect("open")));
    review.with_log(|log| *log = Checkpointer::with_retention_limit(2));

    review.begin_turn_at(1).expect("begin turn");
    review
        .edit(
            "full.txt",
            &[EditOperation {
                old_text: "one".to_owned(),
                new_text: "two".to_owned(),
                replace_all: false,
            }],
        )
        .expect("the write still lands");
    review.seal_turn().expect("seal turn");

    assert_eq!(
        std::fs::read_to_string(root.path().join("full.txt")).expect("read"),
        "two\n",
        "the file changed even though its history could not be kept"
    );
    assert!(
        review
            .with_log(|log| log.history().tracked_paths())
            .is_empty(),
        "nothing was captured, and nothing already captured was dropped to make room"
    );
    assert!(
        review.restorable_paths_at(1).expect("plan").is_empty(),
        "a plan is built from what the log holds, never from what it refused"
    );
    let notice = review
        .take_retention_notice()
        .expect("the refusal is published");
    assert!(
        notice.contains("limit"),
        "the notice names the limit: {notice}"
    );
    assert!(
        review.take_retention_notice().is_none(),
        "the notice is published once"
    );
}

/// A clear or a compaction replaces the message list every turn identifier
/// is a position in, so the log is emptied rather than left pointing at
/// positions that moved. A turn that was running is reopened at the new
/// length, because the tool loop keeps writing after a mid-turn compaction
/// and would otherwise have nothing to record against.
#[test]
fn a_message_list_reset_clears_the_log_and_reopens_a_running_turn() {
    let root = tempdir().expect("workspace");
    let review = seeded(root.path(), "one\n");
    edited_turn(&review, 1, "one\n", "one\ntwo\n");
    assert!(!review.review_state().expect("state").files.is_empty());

    review.reset_messages(0).expect("history cleared");
    let state = review.review_state().expect("state");
    assert!(state.files.is_empty() && state.scopes.is_empty());
    assert!(
        !review.with_log(|log| log.has_open_turn()),
        "no turn was running, so none is reopened"
    );

    review.begin_turn_at(2).expect("begin turn");
    review.reset_messages(7).expect("compacted mid-turn");
    assert!(
        review.with_log(|log| log.has_open_turn()),
        "the turn that was running is reopened"
    );
    review
        .edit(
            "main.txt",
            &[EditOperation {
                old_text: "two\n".to_owned(),
                new_text: "three\n".to_owned(),
                replace_all: false,
            }],
        )
        .expect("the remaining tool loop still records");
    review.seal_turn().expect("seal turn");
    assert_eq!(
        review.with_log(|log| log.history().turn_ids()),
        vec![7],
        "the reopened turn is numbered against the list that replaced the old one"
    );
}

/// A restore that cannot write every path reports the ones it could not and
/// keeps the ones it could: a plan stopped halfway would leave the
/// workspace in a state no version of it ever had.
#[cfg(unix)]
#[test]
fn a_partial_restore_reports_its_failures_and_keeps_the_rest() {
    use std::os::unix::fs::PermissionsExt;

    let root = tempdir().expect("workspace");
    std::fs::create_dir(root.path().join("locked")).expect("locked directory");
    std::fs::write(root.path().join("locked/held.txt"), "one\n").expect("seed file");
    std::fs::write(root.path().join("open.txt"), "one\n").expect("seed file");
    let review = ReviewManager::new(Arc::new(Workspace::open(root.path()).expect("open")));
    review.begin_turn_at(1).expect("begin turn");
    for path in ["locked/held.txt", "open.txt"] {
        review
            .edit(
                path,
                &[EditOperation {
                    old_text: "one\n".to_owned(),
                    new_text: "one\ntwo\n".to_owned(),
                    replace_all: false,
                }],
            )
            .expect("edit");
    }
    review.seal_turn().expect("seal turn");

    // A restore writes in place, as the reference's does, so it is the
    // file that has to refuse the write, not its directory.
    let locked = root.path().join("locked/held.txt");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o444))
        .expect("seal the file");
    let staged = review.stage_restore_to_message(1);
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o644))
        .expect("reopen the file");
    let staged = staged.expect("the restore still answers");

    assert_eq!(staged.errors.len(), 1, "one path could not be written");
    assert!(
        staged.errors[0].contains("locked/held.txt"),
        "the failure names its path: {:?}",
        staged.errors
    );
    assert_eq!(
        staged.transaction.commit(),
        vec![key(root.path(), "open.txt")],
        "the path that could be written was"
    );
    assert_eq!(
        std::fs::read_to_string(root.path().join("open.txt")).expect("read"),
        "one\n"
    );
}

/// The one distinction the port carries, on the implementation the server
/// actually runs: nothing there is absence, and something there that will
/// not open is a failure. Collapsing the second into the first would have
/// the engine record a deletion nobody performed and offer to revert it.
/// A write creates the directories it needs, as the reference's does, so a
/// revert can restore a file whose directory was removed after it.
#[test]
fn the_disk_port_tells_an_absent_path_apart_from_an_unreadable_one() {
    let root = tempdir().expect("workspace");
    let root = crate::platform::canonicalize(root.path()).expect("canonical");
    std::fs::write(root.join("there.txt"), "body\n").expect("seed file");
    std::fs::create_dir(root.join("a-directory")).expect("seed directory");
    let key = |name: &str| root.join(name).to_string_lossy().into_owned();
    let files = DiskFiles;

    assert_eq!(
        files.read_bytes(&key("there.txt")),
        Ok(Some(b"body\n".to_vec()))
    );
    assert_eq!(files.read_bytes(&key("gone.txt")), Ok(None));
    assert_eq!(files.read_bytes(&key("gone/deeper.txt")), Ok(None));
    assert_eq!(files.read_bytes(&key("there.txt/below")), Ok(None));
    assert!(
        files.read_bytes(&key("a-directory")).is_err(),
        "something that is there and will not open is never absence"
    );
    files
        .write_bytes(&key("removed/again/restored.txt"), b"restored\n")
        .expect("a write creates its directories");
    assert_eq!(
        files.read_bytes(&key("removed/again/restored.txt")),
        Ok(Some(b"restored\n".to_vec()))
    );
    assert!(files.exists(&key("removed/again/restored.txt")));
    files
        .remove(&key("removed/again/restored.txt"))
        .expect("remove");
    assert!(!files.exists(&key("removed/again/restored.txt")));
}

/// The engine is fed by the turn boundaries the server already drives, so a
/// session that ran two turns and saw one hand edit carries three owners
/// and can answer what each of them changed.
#[test]
fn the_checkpoint_engine_records_the_turns_the_manager_runs() {
    let root = tempdir().expect("workspace");
    std::fs::write(root.path().join("wired.txt"), "one\n").expect("seed file");
    let review = ReviewManager::new(Arc::new(Workspace::open(root.path()).expect("open")));

    review.begin_turn_at(1).expect("begin turn");
    review
        .edit(
            "wired.txt",
            &[EditOperation {
                old_text: "one\n".to_owned(),
                new_text: "one\ntwo\n".to_owned(),
                replace_all: false,
            }],
        )
        .expect("edit");
    review.seal_turn().expect("seal turn");

    // The user edits the file between the turns, and nothing announces it.
    std::fs::write(root.path().join("wired.txt"), "one\ntwo\nby hand\n").expect("hand edit");

    review.begin_turn_at(2).expect("begin turn");
    review
        .edit(
            "wired.txt",
            &[EditOperation {
                old_text: "by hand\n".to_owned(),
                new_text: "by hand\nthree\n".to_owned(),
                replace_all: false,
            }],
        )
        .expect("edit");
    review.seal_turn().expect("seal turn");

    let owners = review.with_log(|log| {
        log.history()
            .regions(&key(root.path(), "wired.txt"))
            .into_iter()
            .map(|region| region.owner)
            .collect::<Vec<_>>()
    });
    assert_eq!(
        owners,
        vec![
            crate::checkpoints::Owner::Agent { turn_id: 1 },
            crate::checkpoints::Owner::Manual { index: 1 },
            crate::checkpoints::Owner::Agent { turn_id: 2 },
        ],
        "the hand edit keeps its own slot between the two turns"
    );

    let restorable = review.with_log(|log| {
        let plan = log.history().restore_plan_to_turn(2);
        plan.iter()
            .map(|(path, _state)| path.clone())
            .collect::<Vec<_>>()
    });
    assert_eq!(restorable, vec![key(root.path(), "wired.txt")]);
}

/// A rewind drops the turns at and after its point from the log and keeps
/// the ones before, so a later rewind can still reach them.
#[test]
fn a_rewind_truncates_the_engine_log_at_its_point() {
    let root = tempdir().expect("workspace");
    std::fs::write(root.path().join("forked.txt"), "one\n").expect("seed file");
    let review = ReviewManager::new(Arc::new(Workspace::open(root.path()).expect("open")));
    for turn in 1..=3 {
        review.begin_turn_at(turn).expect("begin turn");
        review
            .edit(
                "forked.txt",
                &[EditOperation {
                    old_text: "one\n".to_owned(),
                    new_text: format!("one\nturn {turn}\n"),
                    replace_all: false,
                }],
            )
            .expect("edit");
        review.seal_turn().expect("seal turn");
    }

    review.drop_turns_from(2);

    let turns = review.with_log(|log| {
        let history = log.history();
        (1..=3)
            .filter(|turn| history.has_turn(*turn))
            .collect::<Vec<_>>()
    });
    assert_eq!(turns, vec![1]);
}

// -- The review surface ---------------------------------------------------

/// The key the log files `name` under: its resolved absolute path.
fn key(root: &std::path::Path, name: &str) -> String {
    crate::platform::canonicalize(root)
        .expect("canonical root")
        .join(name)
        .to_string_lossy()
        .into_owned()
}

/// A manager over a fresh workspace holding `seed` at `main.txt`.
fn seeded(root: &std::path::Path, seed: &str) -> ReviewManager {
    std::fs::write(root.join("main.txt"), seed).expect("seed file");
    ReviewManager::new(Arc::new(Workspace::open(root).expect("open")))
}

/// One turn replacing `from` with `to` in `main.txt`.
fn edited_turn(review: &ReviewManager, message_index: usize, from: &str, to: &str) {
    review.begin_turn_at(message_index).expect("begin turn");
    review
        .edit(
            "main.txt",
            &[EditOperation {
                old_text: from.to_owned(),
                new_text: to.to_owned(),
                replace_all: false,
            }],
        )
        .expect("edit");
    review.seal_turn().expect("seal turn");
}

#[test]
fn the_review_surface_lists_the_file_a_turn_changed_with_its_regions() {
    let root = tempdir().expect("workspace");
    let review = seeded(root.path(), "one\n");
    edited_turn(&review, 1, "one\n", "one\ntwo\n");

    let state = review.review_state().expect("review state");
    assert_eq!(state.files.len(), 1);
    assert_eq!(state.files[0].path, key(root.path(), "main.txt"));
    assert_eq!(state.files[0].status, ReviewFileStatus::Modified);
    assert_eq!(state.files[0].regions.len(), 1);
    assert_eq!(
        state.files[0].regions[0].owner(),
        Owner::Agent { turn_id: 1 }
    );
    assert_eq!(state.files[0].regions[0].decision(), Decision::Pending);
    assert_eq!(
        state
            .scopes
            .iter()
            .map(|scope| scope.owner)
            .collect::<Vec<_>>(),
        vec![Owner::Agent { turn_id: 1 }]
    );
    assert_eq!(state.scopes[0].files[0].region_count, 1);
}

/// Rendering is an acting boundary. A hand edit made between two turns is
/// captured when the panel is projected, in its own slot, rather than being
/// absorbed into whichever turn touches the file next.
#[test]
fn rendering_reconciles_a_hand_edit_into_its_own_slot() {
    let root = tempdir().expect("workspace");
    let review = seeded(root.path(), "one\n");
    edited_turn(&review, 1, "one\n", "one\ntwo\n");
    std::fs::write(root.path().join("main.txt"), "one\ntwo\nby hand\n").expect("hand edit");

    let state = review.review_state().expect("review state");
    assert_eq!(
        state
            .scopes
            .iter()
            .map(|scope| scope.owner)
            .collect::<Vec<_>>(),
        vec![Owner::Agent { turn_id: 1 }, Owner::Manual { index: 1 }]
    );
    let again = review.review_state().expect("second render");
    assert_eq!(
        again.scopes.len(),
        2,
        "reconciliation is idempotent, so a second render adds no slot"
    );
}

#[test]
fn approving_leaves_disk_alone_and_reverting_writes_the_file_back() {
    let root = tempdir().expect("workspace");
    let review = seeded(root.path(), "one\n");
    edited_turn(&review, 1, "one\n", "one\ntwo\n");

    let approved = review
        .approve_review(&ReviewTarget::File {
            path: key(root.path(), "main.txt"),
        })
        .expect("approve");
    assert_eq!(approved, vec![key(root.path(), "main.txt")]);
    assert_eq!(
        std::fs::read_to_string(root.path().join("main.txt")).expect("read"),
        "one\ntwo\n",
        "approved content already sits on disk"
    );
    assert!(
        review.review_state().expect("state").files.is_empty(),
        "an approved file has nothing left to review"
    );

    let second = tempdir().expect("workspace");
    let review = seeded(second.path(), "one\n");
    edited_turn(&review, 1, "one\n", "one\ntwo\n");
    let reverted = review
        .revert_review(&ReviewTarget::File {
            path: key(second.path(), "main.txt"),
        })
        .expect("revert");
    assert_eq!(reverted, vec![key(second.path(), "main.txt")]);
    assert_eq!(
        std::fs::read_to_string(second.path().join("main.txt")).expect("read"),
        "one\n",
        "a revert is persisted immediately"
    );
}

/// Deciding is an acting boundary too. A hand edit made since the last look
/// is sealed into its own slot before the decision lands, so reverting the
/// turn the operator was reviewing does not take their work with it.
#[test]
fn deciding_reconciles_a_hand_edit_before_the_decision_lands() {
    let root = tempdir().expect("workspace");
    let review = seeded(root.path(), "one\ntail\n");
    edited_turn(&review, 1, "one\n", "one\ntwo\n");
    // A hand edit nowhere near the turn's line, so nothing drags it.
    std::fs::write(root.path().join("main.txt"), "one\ntwo\ntail\nby hand\n").expect("hand edit");

    review
        .revert_review(&ReviewTarget::Scope {
            owner: Owner::Agent { turn_id: 1 },
        })
        .expect("revert the turn");

    assert_eq!(
        std::fs::read_to_string(root.path().join("main.txt")).expect("read"),
        "one\ntail\nby hand\n",
        "the turn's line is gone and the hand edit is still there"
    );
    let state = review.review_state().expect("review state");
    assert_eq!(
        state
            .scopes
            .iter()
            .map(|scope| scope.owner)
            .collect::<Vec<_>>(),
        vec![Owner::Agent { turn_id: 1 }, Owner::Manual { index: 1 }],
        "the hand edit kept its own slot rather than being reverted with the turn"
    );
}

/// Anchoring is an acting boundary for the same reason: the whole-file
/// anchors have to line up with the content the panel renders, which is
/// disk, so a drift is folded in before the diff is taken.
#[test]
fn anchoring_reconciles_before_it_locates_the_pending_hunks() {
    let root = tempdir().expect("workspace");
    let review = seeded(root.path(), "one\n");
    edited_turn(&review, 1, "one\n", "one\ntwo\n");
    std::fs::write(root.path().join("main.txt"), "one\ntwo\nby hand\n").expect("hand edit");

    let hunks = review
        .file_hunks(&key(root.path(), "main.txt"), None)
        .expect("hunks");
    assert_eq!(
        hunks.len(),
        1,
        "the turn's line and the hand edit render as one adjacent block"
    );
    assert_eq!(hunks[0].side, HunkSide::Additions);
    assert_eq!(
        hunks[0].line, 2,
        "the anchor sits on the last line of the rendered block, the hand edit's"
    );
    assert_eq!(
        hunks[0].regions.len(),
        2,
        "a control there decides the turn's hunk and the hand edit together"
    );
}

/// A decision is durable only once what it implies on disk has landed. When
/// the write fails the log goes back to what it was, so the surface never
/// reports a region decided against a file that did not change.
#[cfg(unix)]
#[test]
fn a_write_that_does_not_land_rolls_the_decision_back() {
    use std::os::unix::fs::PermissionsExt;

    let root = tempdir().expect("workspace");
    std::fs::create_dir(root.path().join("locked")).expect("locked directory");
    std::fs::write(root.path().join("locked/main.txt"), "one\n").expect("seed file");
    let review = ReviewManager::new(Arc::new(Workspace::open(root.path()).expect("open")));
    review.begin_turn_at(1).expect("begin turn");
    review
        .edit(
            "locked/main.txt",
            &[EditOperation {
                old_text: "one\n".to_owned(),
                new_text: "one\ntwo\n".to_owned(),
                replace_all: false,
            }],
        )
        .expect("edit");
    review.seal_turn().expect("seal turn");

    let locked = root.path().join("locked/main.txt");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o444))
        .expect("seal the file");
    let failure = review.revert_review(&ReviewTarget::File {
        path: key(root.path(), "locked/main.txt"),
    });
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o644))
        .expect("reopen the file");

    assert!(
        matches!(failure, Err(ReviewError::Persist(_))),
        "the write failure is reported: {failure:?}"
    );
    let state = review.review_state().expect("review state");
    assert_eq!(
        state.files.len(),
        1,
        "the file is still pending, so the decision did not stick"
    );
    assert_eq!(
        state.files[0].regions[0].decision(),
        Decision::Pending,
        "the log was rolled back to before the refused decision"
    );
}

#[test]
fn the_baseline_the_scope_diff_and_the_hunks_answer_from_the_log() {
    let root = tempdir().expect("workspace");
    let review = seeded(root.path(), "one\n");
    edited_turn(&review, 1, "one\n", "one\ntwo\n");

    let main = key(root.path(), "main.txt");
    assert_eq!(
        review.baseline_text(&main).expect("baseline"),
        "one\n",
        "nothing is kept yet, so the accepted baseline is the original"
    );
    assert_eq!(
        review
            .baseline_text(&key(root.path(), "untracked.txt"))
            .expect("baseline"),
        "",
        "a path the log never saw answers empty rather than failing"
    );

    let diff = review
        .scope_file_diff(&main, Owner::Agent { turn_id: 1 })
        .expect("scope diff");
    assert_eq!(diff.status, ReviewFileStatus::Modified);
    assert_eq!(diff.baseline, "one\n");
    assert_eq!(diff.current, "one\ntwo\n");

    let hunks = review.file_hunks(&main, None).expect("hunks");
    assert_eq!(hunks.len(), 1);
    assert_eq!(hunks[0].side, HunkSide::Additions);
    assert_eq!(hunks[0].line, 1);
    assert_eq!(hunks[0].regions.len(), 1);

    let scoped = review
        .file_hunks(&main, Some(Owner::Agent { turn_id: 1 }))
        .expect("scoped hunks");
    assert_eq!(scoped, hunks, "the one turn's scope is the whole diff");
    assert!(
        review
            .file_hunks(&main, Some(Owner::Agent { turn_id: 9 }))
            .expect("absent owner")
            .is_empty(),
        "an owner with nothing pending anchors nothing"
    );
}

/// The rewind budget admits a capture of exactly its size and nothing past
/// it: a file one byte over is refused whole, and once a turn has spent the
/// budget even a one-byte file is. The large files are sparse, so the
/// fixture costs no disk.
#[test]
fn the_rewind_budget_admits_its_exact_size_and_nothing_more() {
    let root = tempdir().expect("workspace");
    let sized = |name: &str, len: usize| {
        let file = std::fs::File::create(root.path().join(name)).expect("create file");
        file.set_len(u64::try_from(len).expect("length fits"))
            .expect("size file");
    };
    sized("over.bin", MAX_REWIND_SNAPSHOT_BYTES + 1);
    sized("at_limit.bin", MAX_REWIND_SNAPSHOT_BYTES);
    std::fs::write(root.path().join("one_more.txt"), "x").expect("seed file");
    let review = ReviewManager::new(Arc::new(Workspace::open(root.path()).expect("open")));
    review.begin_turn_at(1).expect("begin turn");

    assert!(matches!(
        review.capture_before_mutation("over.bin"),
        Err(WorkspaceError::ReviewSnapshotLimit { actual, limit })
            if actual == MAX_REWIND_SNAPSHOT_BYTES + 1 && limit == MAX_REWIND_SNAPSHOT_BYTES
    ));
    review
        .capture_before_mutation("at_limit.bin")
        .expect("a capture of exactly the budget fits");
    assert!(matches!(
        review.capture_before_mutation("one_more.txt"),
        Err(WorkspaceError::ReviewSnapshotLimit {
            actual: 1,
            limit: 0
        })
    ));
}
