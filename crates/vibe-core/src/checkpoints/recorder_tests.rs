//! What the write shell reads, when it reads it, and what a failed read costs.

use super::checkpointer::Checkpointer;
use super::files::FileStore;
use super::files_tests::FakeFiles;
use super::lines::FileState;
use super::models::Owner;
use super::recorder::{CheckpointRecorder, RecorderError};

fn text(value: &str) -> FileState {
    FileState::from_text(value)
}

fn recorder(files: &FakeFiles) -> CheckpointRecorder<&FakeFiles> {
    CheckpointRecorder::new(FileStore::new(files))
}

/// Every region of `path`, as its owner and the text it produced.
fn produced(log: &Checkpointer, path: &str) -> (Vec<Owner>, Option<String>) {
    let history = log.history();
    (
        history
            .regions(path)
            .into_iter()
            .map(|region| region.owner)
            .collect(),
        history
            .content(path)
            .data()
            .map(|bytes| String::from_utf8_lossy(bytes).into_owned()),
    )
}

// -- US-131: driving the turn lifecycle --------------------------------------

#[test]
fn a_turn_re_reads_what_the_previous_turn_tracked() {
    let files = FakeFiles::new(&[("tracked", "one\n")]);
    let recorder = recorder(&files);
    let mut log = Checkpointer::new();

    recorder.create_checkpoint(&mut log, 1).unwrap();
    recorder
        .add_snapshot(&mut log, "tracked", text("one\n"))
        .unwrap();
    files.put("tracked", "one\ntwo\n");
    recorder.seal_turn(&mut log);

    // The second turn announces nothing, and a tool mutates the file anyway.
    recorder.create_checkpoint(&mut log, 2).unwrap();
    files.put("tracked", "one\ntwo\nthree\n");
    assert!(recorder.seal_turn(&mut log).is_empty());

    assert_eq!(
        produced(&log, "tracked"),
        (
            vec![Owner::Agent { turn_id: 1 }, Owner::Agent { turn_id: 2 }],
            Some("one\ntwo\nthree\n".to_owned())
        ),
        "the re-read is what attributes the silent change to the turn that made it"
    );
}

#[test]
fn a_snapshot_handed_in_becomes_the_paths_state_for_the_turn() {
    let files = FakeFiles::new(&[("f", "written by the tool\n")]);
    let recorder = recorder(&files);
    let mut log = Checkpointer::new();

    recorder.create_checkpoint(&mut log, 1).unwrap();
    // The tool read the file, then wrote it, then reports what it read.
    recorder
        .add_snapshot(&mut log, "f", text("before the tool\n"))
        .unwrap();
    recorder.seal_turn(&mut log);

    assert_eq!(log.history().original("f"), text("before the tool\n"));
    assert_eq!(
        produced(&log, "f"),
        (
            vec![Owner::Agent { turn_id: 1 }],
            Some("written by the tool\n".to_owned())
        )
    );
}

#[test]
fn one_unreadable_path_at_seal_is_reported_and_the_others_still_record() {
    let files = FakeFiles::new(&[("locked", "before\n"), ("open", "before\n")]);
    let recorder = recorder(&files);
    let mut log = Checkpointer::new();

    recorder.create_checkpoint(&mut log, 1).unwrap();
    recorder
        .add_snapshot(&mut log, "locked", text("before\n"))
        .unwrap();
    recorder
        .add_snapshot(&mut log, "open", text("before\n"))
        .unwrap();
    files.put("open", "after\n");
    files.put("locked", "after\n");
    files.refuse_reads("locked");

    let failures = recorder.seal_turn(&mut log);

    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].path, "locked");
    assert!(
        !log.has_open_turn(),
        "the turn closes whatever the reads answered"
    );
    assert_eq!(
        produced(&log, "open"),
        (
            vec![Owner::Agent { turn_id: 1 }],
            Some("after\n".to_owned())
        ),
        "the readable path sealed its change"
    );
    assert_eq!(
        produced(&log, "locked"),
        (Vec::new(), Some("before\n".to_owned())),
        "the unreadable path recorded nothing rather than an empty change"
    );
}

#[test]
fn a_turn_seals_even_when_every_read_fails() {
    let files = FakeFiles::new(&[("a", "before\n"), ("b", "before\n")]);
    let recorder = recorder(&files);
    let mut log = Checkpointer::new();

    recorder.create_checkpoint(&mut log, 1).unwrap();
    recorder
        .add_snapshot(&mut log, "a", text("before\n"))
        .unwrap();
    recorder
        .add_snapshot(&mut log, "b", text("before\n"))
        .unwrap();
    files.refuse_reads("a");
    files.refuse_reads("b");

    let failures = recorder.seal_turn(&mut log);

    assert_eq!(failures.len(), 2);
    assert!(!log.has_open_turn());
}

#[test]
fn a_turn_that_begins_while_one_is_open_is_refused_by_the_log() {
    let files = FakeFiles::new(&[]);
    let recorder = recorder(&files);
    let mut log = Checkpointer::new();
    recorder.create_checkpoint(&mut log, 1).unwrap();

    let error = recorder
        .create_checkpoint(&mut log, 2)
        .expect_err("the log owns the turn gate");

    assert!(matches!(error, RecorderError::Log(_)), "{error}");
}

#[test]
fn a_carried_path_that_cannot_be_read_fails_the_turn_start() {
    let files = FakeFiles::new(&[("tracked", "one\n")]);
    let recorder = recorder(&files);
    let mut log = Checkpointer::new();
    recorder.create_checkpoint(&mut log, 1).unwrap();
    recorder
        .add_snapshot(&mut log, "tracked", text("one\n"))
        .unwrap();
    recorder.seal_turn(&mut log);
    files.refuse_reads("tracked");

    let error = recorder
        .create_checkpoint(&mut log, 2)
        .expect_err("an unreadable carried path is not an absent one");

    assert!(matches!(error, RecorderError::File(_)), "{error}");
}

#[test]
fn a_hand_edit_made_between_turns_is_captured_when_the_next_turn_reads_the_path() {
    let files = FakeFiles::new(&[("f", "agent\n")]);
    let recorder = recorder(&files);
    let mut log = Checkpointer::new();

    recorder.create_checkpoint(&mut log, 1).unwrap();
    recorder
        .add_snapshot(&mut log, "f", text("original\n"))
        .unwrap();
    recorder.seal_turn(&mut log);
    // The user edits the file, and nothing tells the engine until the next turn
    // re-reads it.
    files.put("f", "agent\nby hand\n");
    recorder.create_checkpoint(&mut log, 2).unwrap();
    files.put("f", "agent\nby hand\nagent again\n");
    recorder.seal_turn(&mut log);

    assert_eq!(
        produced(&log, "f").0,
        vec![
            Owner::Agent { turn_id: 1 },
            Owner::Manual { index: 1 },
            Owner::Agent { turn_id: 2 },
        ]
    );
}

#[test]
fn a_restore_plan_applied_through_the_store_puts_the_files_back() {
    let files = FakeFiles::new(&[("f", "original\n")]);
    let recorder = recorder(&files);
    let mut log = Checkpointer::new();

    recorder.create_checkpoint(&mut log, 1).unwrap();
    recorder
        .add_snapshot(&mut log, "f", text("original\n"))
        .unwrap();
    files.put("f", "rewritten\n");
    recorder.seal_turn(&mut log);

    let plan = log.history().restore_plan_to_turn(1);
    assert_eq!(recorder.files().diverging_paths(&plan), vec!["f"]);
    let outcome = recorder.files().apply(&plan);

    assert_eq!(outcome.restored, vec!["f"]);
    assert!(outcome.errors.is_empty());
    assert_eq!(files.text_at("f").as_deref(), Some("original\n"));
    log.drop_turns_from(1);
    assert!(log.history().tracked_paths().is_empty());
}
