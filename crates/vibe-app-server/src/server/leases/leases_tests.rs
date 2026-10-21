use super::*;

fn leases() -> Leases {
    Arc::new(Mutex::new(BTreeMap::new()))
}

/// Whether another holder is refused the lease on `session_id`, which is what
/// a second process opening the session sees.
fn busy(root: &Path, session_id: &str) -> bool {
    matches!(
        SessionLease::acquire(root, session_id),
        Err(LeaseError::Busy(_))
    )
}

/// An opening that takes the lease and then fails a later step.
fn failing_opening(leases: &Leases, root: &Path, session_id: &str) -> Result<(), LeaseError> {
    let _lease = LeaseGuard::take(leases, root, session_id)?;
    Err(LeaseError::InvalidSessionId(
        "a later step failed".to_owned(),
    ))
}

#[test]
fn an_error_after_acquiring_releases_the_lease() {
    let root = tempfile::tempdir().expect("save directory");
    let leases = leases();
    assert!(failing_opening(&leases, root.path(), "session-1").is_err());
    assert!(lock(&leases).is_empty());
    assert!(!busy(root.path(), "session-1"));
}

#[test]
fn a_kept_lease_stays_held_until_released() {
    let root = tempfile::tempdir().expect("save directory");
    let leases = leases();
    let guard = LeaseGuard::take(&leases, root.path(), "session-1").expect("free lease");
    assert!(busy(root.path(), "session-1"));
    guard.keep();
    assert!(lock(&leases).contains_key("session-1"));
    assert!(busy(root.path(), "session-1"));
    let lease = lock(&leases).remove("session-1");
    drop(lease);
    assert!(!busy(root.path(), "session-1"));
}

#[test]
fn a_guard_over_a_lease_held_already_releases_nothing() {
    let root = tempfile::tempdir().expect("save directory");
    let leases = leases();
    LeaseGuard::take(&leases, root.path(), "session-1")
        .expect("free lease")
        .keep();
    assert!(failing_opening(&leases, root.path(), "session-1").is_err());
    assert!(lock(&leases).contains_key("session-1"));
    assert!(busy(root.path(), "session-1"));
}

#[test]
fn a_lease_another_holder_has_is_refused_as_a_conflict() {
    let root = tempfile::tempdir().expect("save directory");
    let _other = SessionLease::acquire(root.path(), "session-1").expect("free lease");
    let refusal = LeaseGuard::take(&leases(), root.path(), "session-1")
        .err()
        .expect("a busy lease");
    let fault = ProtocolFault::from(refusal);
    assert!(matches!(
        fault,
        ProtocolFault::Other {
            code: ProtocolErrorCode::Conflict,
            ..
        }
    ));
}
