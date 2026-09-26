//! Session leases: `active/<id>.lock` and its diagnostic.

use super::lease::{ACTIVE_DIRECTORY, LeaseError, SessionLease};
use super::*;

fn lock_paths(root: &Path, id: &str) -> (PathBuf, PathBuf) {
    let active = root.join(ACTIVE_DIRECTORY);
    (
        active.join(format!("{id}.lock")),
        active.join(format!("{id}.lock.json")),
    )
}

#[test]
fn acquire_writes_the_lock_and_its_diagnostic() {
    let temporary = tempfile::tempdir().expect("temporary session root");
    let lease = SessionLease::acquire(temporary.path(), "lease-session").expect("lease taken");
    assert_eq!(lease.session_id(), "lease-session");
    let (lock, diagnostic) = lock_paths(temporary.path(), "lease-session");
    assert!(lock.is_file());

    let text = fs::read_to_string(&diagnostic).expect("diagnostic reads");
    assert!(text.ends_with('\n'));
    assert_eq!(text.lines().count(), 1);
    assert!(text.starts_with("{\"acquired_at\":"));
    let record: Value = serde_json::from_str(&text).expect("diagnostic parses");
    let object = record.as_object().expect("diagnostic is an object");
    assert_eq!(
        object.keys().map(String::as_str).collect::<Vec<_>>(),
        ["acquired_at", "lease_version", "process_id", "session_id"]
    );
    assert_eq!(record["lease_version"], 1);
    assert_eq!(record["process_id"], std::process::id());
    assert_eq!(record["session_id"], "lease-session");
    let acquired_at = record["acquired_at"].as_str().expect("acquired_at is text");
    assert_eq!(acquired_at.len(), "2024-01-02T03:04:05.678Z".len());
    assert!(acquired_at.ends_with('Z'));
    assert!(parse_iso_millis(acquired_at).is_some());
    drop(lease);
}

#[test]
fn a_second_acquire_of_a_held_session_is_busy() {
    let temporary = tempfile::tempdir().expect("temporary session root");
    let _held = SessionLease::acquire(temporary.path(), "busy-session").expect("lease taken");

    let error =
        SessionLease::acquire(temporary.path(), "busy-session").expect_err("the session is held");
    assert!(matches!(&error, LeaseError::Busy(id) if id == "busy-session"));
    assert_eq!(error.to_string(), "Session is already open: busy-session");
    let (lock, diagnostic) = lock_paths(temporary.path(), "busy-session");
    assert!(lock.is_file());
    assert!(diagnostic.is_file());

    let _other = SessionLease::acquire(temporary.path(), "other-session")
        .expect("another session is independent");
}

#[test]
fn releasing_removes_both_files_and_allows_reacquiring() {
    let temporary = tempfile::tempdir().expect("temporary session root");
    let (lock, diagnostic) = lock_paths(temporary.path(), "released-session");

    SessionLease::acquire(temporary.path(), "released-session")
        .expect("lease taken")
        .release();
    assert!(!lock.exists());
    assert!(!diagnostic.exists());

    {
        let _scoped =
            SessionLease::acquire(temporary.path(), "released-session").expect("reacquired");
        assert!(lock.is_file());
    }
    assert!(!lock.exists());
    assert!(!diagnostic.exists());

    let again =
        SessionLease::acquire(temporary.path(), "released-session").expect("reacquired after drop");
    assert!(diagnostic.is_file());
    again.release();
}

#[test]
fn invalid_session_ids_are_refused_before_anything_is_written() {
    let temporary = tempfile::tempdir().expect("temporary session root");
    let too_long = "a".repeat(129);
    for id in [
        "",
        "../escape",
        "a/b",
        "-leading-dash",
        "_leading-underscore",
        "dot.dot",
        too_long.as_str(),
    ] {
        assert!(
            matches!(
                SessionLease::acquire(temporary.path(), id),
                Err(LeaseError::InvalidSessionId(refused)) if refused == id
            ),
            "{id:?} is refused"
        );
    }
    assert!(!temporary.path().join(ACTIVE_DIRECTORY).exists());

    let longest = "a".repeat(128);
    SessionLease::acquire(temporary.path(), &longest)
        .expect("128 characters is allowed")
        .release();
}

#[cfg(unix)]
#[test]
fn a_symbolic_link_in_the_lease_path_is_refused() {
    let temporary = tempfile::tempdir().expect("temporary session root");
    let root = temporary.path().join("root");
    let target = temporary.path().join("target");
    fs::create_dir(&root).expect("root");
    fs::create_dir(&target).expect("target");
    std::os::unix::fs::symlink(&target, root.join(ACTIVE_DIRECTORY)).expect("symlink");

    assert!(matches!(
        SessionLease::acquire(&root, "linked-session"),
        Err(LeaseError::SymbolicLink)
    ));
    assert_eq!(fs::read_dir(&target).expect("target lists").count(), 0);
}
