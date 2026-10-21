use super::*;

use notify::event::{CreateKind, ModifyKind, RemoveKind};

fn event(kind: EventKind, paths: &[&str]) -> notify::Event {
    notify::Event {
        kind,
        paths: paths.iter().map(PathBuf::from).collect(),
        attrs: notify::event::EventAttributes::default(),
    }
}

#[test]
fn platform_events_map_onto_the_three_applied_categories() {
    let mut batch = Vec::new();
    collect(
        &event(EventKind::Create(CreateKind::File), &["/w/a"]),
        &mut batch,
    );
    collect(
        &event(
            EventKind::Modify(ModifyKind::Data(notify::event::DataChange::Any)),
            &["/w/b"],
        ),
        &mut batch,
    );
    collect(
        &event(EventKind::Remove(RemoveKind::File), &["/w/c"]),
        &mut batch,
    );
    collect(
        &event(
            EventKind::Modify(ModifyKind::Name(RenameMode::From)),
            &["/w/d"],
        ),
        &mut batch,
    );
    collect(
        &event(
            EventKind::Modify(ModifyKind::Name(RenameMode::To)),
            &["/w/e"],
        ),
        &mut batch,
    );
    collect(
        &event(
            EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
            &["/w/f", "/w/g"],
        ),
        &mut batch,
    );
    collect(
        &event(
            EventKind::Access(notify::event::AccessKind::Read),
            &["/w/h"],
        ),
        &mut batch,
    );
    collect(&event(EventKind::Any, &["/w/i"]), &mut batch);

    assert_eq!(
        batch,
        vec![
            (ChangeKind::Added, PathBuf::from("/w/a")),
            (ChangeKind::Modified, PathBuf::from("/w/b")),
            (ChangeKind::Deleted, PathBuf::from("/w/c")),
            (ChangeKind::Deleted, PathBuf::from("/w/d")),
            (ChangeKind::Added, PathBuf::from("/w/e")),
            (ChangeKind::Deleted, PathBuf::from("/w/f")),
            (ChangeKind::Added, PathBuf::from("/w/g")),
        ]
    );
}

#[test]
fn a_started_watch_is_not_restarted_for_the_same_root() {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let root = vibe_core::platform::canonicalize(workspace.path()).expect("resolved root");
    let mut controller = WatchController::default();

    controller.start(&root).expect("the backend is available");
    assert!(controller.is_watching());
    let first = controller
        .active
        .as_ref()
        .map(|watch| watch.thread.thread().id());
    controller.start(&root).expect("the same root is kept");
    let second = controller
        .active
        .as_ref()
        .map(|watch| watch.thread.thread().id());
    assert_eq!(first, second, "the same root keeps the same thread");

    controller.stop();
    assert!(!controller.is_watching());
}

#[test]
fn a_second_root_replaces_the_first_and_stop_returns_within_the_join_bound() {
    let first = tempfile::tempdir().expect("first workspace");
    let second = tempfile::tempdir().expect("second workspace");
    let first = vibe_core::platform::canonicalize(first.path()).expect("resolved root");
    let second = vibe_core::platform::canonicalize(second.path()).expect("resolved root");
    let mut controller = WatchController::default();

    controller.start(&first).expect("the backend is available");
    controller.start(&second).expect("the backend is available");
    assert_eq!(
        controller.active.as_ref().map(|watch| watch.root.clone()),
        Some(second)
    );

    let started = Instant::now();
    controller.stop();
    assert!(
        started.elapsed() < JOIN_TIMEOUT.saturating_mul(2),
        "stop returns within the join bound"
    );
    assert!(!controller.is_watching());
}

#[test]
fn an_unwatchable_root_reports_its_cause_and_starts_nothing() {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let missing = workspace.path().join("absent");
    let mut controller = WatchController::default();

    let failure = controller.start(&missing);

    assert!(failure.is_err(), "an absent root cannot be watched");
    assert!(!controller.is_watching());
    assert!(controller.next_batch().is_none());
}

#[test]
fn a_written_file_reaches_a_delivered_batch() {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let root = vibe_core::platform::canonicalize(workspace.path()).expect("resolved root");
    let mut controller = WatchController::default();
    controller.start(&root).expect("the backend is available");

    std::fs::write(root.join("appeared.txt"), "fixture").expect("watched write");

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut observed = Vec::new();
    while Instant::now() < deadline && observed.is_empty() {
        if let Some((batch_root, changes)) = controller.next_batch() {
            assert_eq!(batch_root, root, "a batch names the root it was seen under");
            observed = changes;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    controller.stop();

    assert!(
        observed
            .iter()
            .any(|(kind, path)| *kind == ChangeKind::Added
                && path.file_name().is_some_and(|name| name == "appeared.txt")),
        "the written file is reported as an addition: {observed:?}"
    );
}
