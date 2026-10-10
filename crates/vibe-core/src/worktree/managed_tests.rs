//! The claim store as one process sees it: holders, the attachment hold, and
//! what forgetting a claim leaves behind. The corpus drives the outcomes; these
//! cases hold the bookkeeping underneath them.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::{
    ManagedRoot, ManagedWorktree, PreparedWorktree, SNAPSHOT_REF_PREFIX, WorktreeError,
    WorktreeReleaseOutcome, WorktreeRepository,
};

fn git(directory: &Path, arguments: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(arguments)
        .output()
        .expect("git is on PATH")
        .status;
    assert!(status.success(), "git {arguments:?} failed");
}

/// A committed checkout and the managed root beside it.
fn fixture() -> (tempfile::TempDir, PathBuf, ManagedRoot) {
    let scratch = tempfile::tempdir().expect("tempdir");
    let root = scratch.path().canonicalize().expect("the root resolves");
    let checkout = root.join("repo");
    fs::create_dir_all(&checkout).expect("the checkout is writable");
    git(&checkout, &["init", "--quiet", "--initial-branch", "main"]);
    git(&checkout, &["config", "user.name", "Vibe Test"]);
    git(&checkout, &["config", "user.email", "vibe@example.test"]);
    git(&checkout, &["config", "commit.gpgsign", "false"]);
    fs::write(checkout.join("README.md"), "fixture\n").expect("the fixture is writable");
    git(&checkout, &["add", "--all"]);
    git(
        &checkout,
        &["commit", "--quiet", "--no-gpg-sign", "-m", "fixture"],
    );
    let managed = ManagedRoot::for_vibe_home(&root.join("home"));
    (scratch, checkout, managed)
}

fn prepared(
    checkout: &Path,
    managed: &ManagedRoot,
    name: &str,
) -> (PreparedWorktree, ManagedWorktree) {
    let prepared = WorktreeRepository::open(checkout, managed)
        .and_then(|repository| repository.prepare(name, None))
        .expect("the worktree is prepared");
    let held = ManagedWorktree::at(managed, &prepared.path).expect("the worktree is managed");
    (prepared, held)
}

#[test]
fn a_fresh_worktree_is_starting_until_a_session_holds_it() {
    let (_scratch, checkout, managed) = fixture();
    let (prepared, held) = prepared(&checkout, &managed, "review");
    assert!(held.claim().is_starting());
    assert!(held.holders().is_empty(), "the marker is not a holder");

    held.hold("s1", prepared.pending_hold.as_ref())
        .expect("held");
    assert!(!held.claim().is_starting());
    assert_eq!(held.holders().into_iter().collect::<Vec<_>>(), ["s1"]);
    held.release_holder("s1").expect("released");
    assert!(held.holders().is_empty());
}

#[test]
fn an_attachment_hold_counts_until_it_is_released() {
    let (_scratch, checkout, managed) = fixture();
    let (prepared, held) = prepared(&checkout, &managed, "review");
    held.hold("s1", prepared.pending_hold.as_ref())
        .expect("held");
    held.release_holder("s1").expect("released");

    let pending = held
        .hold_for_attachment()
        .expect("the hold is taken")
        .expect("a recorded worktree can be held");
    assert_eq!(held.holders().len(), 1);
    pending.release();
    assert!(held.holders().is_empty());
}

#[test]
fn a_pending_hold_for_another_worktree_is_refused_and_released() {
    let (_scratch, checkout, managed) = fixture();
    let (first, first_held) = prepared(&checkout, &managed, "first");
    first_held
        .hold("s1", first.pending_hold.as_ref())
        .expect("held");
    let (second, second_held) = prepared(&checkout, &managed, "second");
    second_held
        .hold("s2", second.pending_hold.as_ref())
        .expect("held");

    let pending = first_held
        .hold_for_attachment()
        .expect("the hold is taken")
        .expect("a recorded worktree can be held");
    let refused = second_held.hold("s3", Some(&pending));
    assert!(matches!(refused, Err(WorktreeError::Failed { .. })));
    assert_eq!(
        first_held.holders().into_iter().collect::<Vec<_>>(),
        ["s1"],
        "the mismatched hold was released"
    );
}

#[test]
fn a_forgotten_claim_holds_nothing_and_its_last_holder_cleans_up() {
    let (_scratch, checkout, managed) = fixture();
    let (prepared, held) = prepared(&checkout, &managed, "review");
    held.hold("s1", prepared.pending_hold.as_ref())
        .expect("held");
    held.forget();
    assert!(held.claim().read().is_none());
    assert!(held.hold_for_attachment().expect("no error").is_none());

    held.release_holder("s1").expect("released");
    assert!(
        !held.claim().directory().exists(),
        "the last holder out of a forgotten claim removes its directory"
    );
}

#[test]
fn a_held_worktree_keeps_its_reap_until_the_sweep_finds_it_unheld() {
    let (_scratch, checkout, managed) = fixture();
    let (prepared, held) = prepared(&checkout, &managed, "review");
    held.hold("s1", prepared.pending_hold.as_ref())
        .expect("held");

    let kept = held.reap(Some(("desktop", "r1"))).expect("the reap runs");
    assert_eq!(kept.outcome, WorktreeReleaseOutcome::KeptInUse);
    let record = held.claim().read().expect("the claim stays");
    assert!(record.reap_requested);
    assert_eq!(
        record.reap_requests.get("desktop").map(String::as_str),
        Some("r1")
    );

    held.release_holder("s1").expect("released");
    let swept = ManagedWorktree::prune(&managed, 15).expect("the sweep runs");
    assert_eq!(swept, 1, "a pending reap is finished under the limit too");
    assert!(!prepared.path.exists());
    let recovery = held
        .claim()
        .read_recovery()
        .expect("a reap records how to recover the work");
    assert!(recovery.snapshot_ref.starts_with(SNAPSHOT_REF_PREFIX));
}

#[test]
fn a_cancelled_reap_refuses_the_same_request_arriving_late() {
    let (_scratch, checkout, managed) = fixture();
    let (prepared, held) = prepared(&checkout, &managed, "review");
    held.hold("s1", prepared.pending_hold.as_ref())
        .expect("held");
    held.reap(Some(("desktop", "r1"))).expect("the reap runs");
    held.reap(Some(("cli", "r2"))).expect("the reap runs");

    held.cancel_reap(Some("desktop"), None)
        .expect("the reap is withdrawn");
    let record = held.claim().read().expect("the claim stays");
    assert!(record.reap_requested, "the other requester's reap stands");
    assert!(record.reap_cancellations.contains("r1"));

    held.release_holder("s1").expect("released");
    let late = held.reap(Some(("desktop", "r1"))).expect("the reap runs");
    assert_eq!(late.outcome, WorktreeReleaseOutcome::KeptCancelled);
    assert!(prepared.path.exists());

    held.cancel_reap(None, None)
        .expect("every reap is withdrawn");
    let record = held.claim().read().expect("the claim stays");
    assert!(!record.reap_requested && record.reap_requests.is_empty());
    assert!(held.reap_if_requested().expect("no error").is_none());
}

#[test]
fn an_unheld_reap_snapshots_a_clean_worktree_before_removing_it() {
    let (_scratch, checkout, managed) = fixture();
    let (prepared, held) = prepared(&checkout, &managed, "review");
    held.hold("s1", prepared.pending_hold.as_ref())
        .expect("held");
    held.release_holder("s1").expect("released");

    let reaped = held.reap(None).expect("the reap runs");
    assert_eq!(reaped.outcome, WorktreeReleaseOutcome::Removed);
    assert!(
        reaped.snapshot_ref.is_some(),
        "a reap always saves the state"
    );
    assert!(!prepared.path.exists());
}

#[test]
fn a_probe_discounts_the_asker_and_a_forced_release_drops_the_work() {
    let (_scratch, checkout, managed) = fixture();
    let (prepared, held) = prepared(&checkout, &managed, "review");
    held.hold("s1", prepared.pending_hold.as_ref())
        .expect("held");
    fs::write(prepared.path.join("draft.txt"), "unsaved\n").expect("the worktree is writable");

    let other = held.probe_release(Some("s2")).expect("the probe runs");
    assert_eq!(other.outcome, WorktreeReleaseOutcome::KeptInUse);
    let own = held.probe_release(Some("s1")).expect("the probe runs");
    assert_eq!(own.outcome, WorktreeReleaseOutcome::KeptDirty);
    assert!(prepared.path.exists(), "a probe changes nothing");

    let forced = held
        .force_release(Some("s1"), Some(false))
        .expect("the release runs");
    assert_eq!(forced.outcome, WorktreeReleaseOutcome::Removed);
    assert!(!forced.branch_deleted);
    assert!(forced.snapshot_ref.is_none());
    assert!(!prepared.path.exists());
}
