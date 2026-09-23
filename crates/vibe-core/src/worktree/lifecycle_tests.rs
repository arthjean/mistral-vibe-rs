//! The session lifecycle over paths: what a request resolves to, and what a
//! failed start takes back.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::ManagedRoot;
use super::lifecycle::{LifecycleError, SessionWorktrees, WorktreeRequest};

fn git(directory: &Path, arguments: &[&str]) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(arguments)
        .output()
        .expect("git is on PATH")
        .status
        .success()
}

fn fixture() -> (tempfile::TempDir, PathBuf, SessionWorktrees) {
    let scratch = tempfile::tempdir().expect("tempdir");
    let root = scratch.path().canonicalize().expect("the root resolves");
    let checkout = root.join("repo");
    fs::create_dir_all(&checkout).expect("the checkout is writable");
    for arguments in [
        &["init", "--quiet", "--initial-branch", "main"][..],
        &["config", "user.name", "Vibe Test"],
        &["config", "user.email", "vibe@example.test"],
        &["config", "commit.gpgsign", "false"],
    ] {
        assert!(git(&checkout, arguments));
    }
    fs::write(checkout.join("README.md"), "fixture\n").expect("the fixture is writable");
    assert!(git(&checkout, &["add", "--all"]));
    assert!(git(
        &checkout,
        &["commit", "--quiet", "--no-gpg-sign", "-m", "fixture"]
    ));
    let lifecycle = SessionWorktrees::new(ManagedRoot::for_vibe_home(&root.join("home")));
    (scratch, checkout, lifecycle)
}

#[test]
fn a_base_that_is_not_a_directory_is_refused() {
    let (_scratch, checkout, lifecycle) = fixture();
    let refused = lifecycle.resolve(
        &WorktreeRequest::CreateForPrompt { prompt: None },
        &checkout.join("absent"),
        None,
    );
    assert!(matches!(refused, Err(LifecycleError::BaseNotADirectory(_))));
}

#[test]
fn an_existing_request_must_name_a_linked_worktree() {
    let (_scratch, checkout, lifecycle) = fixture();
    let refused = lifecycle.resolve(
        &WorktreeRequest::UseExisting {
            cwd: checkout.clone(),
        },
        &checkout,
        None,
    );
    assert!(matches!(refused, Err(LifecycleError::NotLinked(_))));
}

#[test]
fn a_prompt_request_takes_the_suggestion_on_a_vibe_branch() {
    let (_scratch, checkout, lifecycle) = fixture();
    let resolved = lifecycle
        .resolve(
            &WorktreeRequest::CreateForPrompt {
                prompt: Some("Fix the login redirect".to_owned()),
            },
            &checkout,
            Some("Redirect fix"),
        )
        .expect("the worktree is raised");
    let prepared = resolved.prepared.expect("a created worktree");
    assert_eq!(prepared.name, "redirect-fix");
    assert_eq!(prepared.branch, "vibe/redirect-fix");
    assert_eq!(resolved.cwd, prepared.path);
    assert!(lifecycle.is_managed(&resolved.cwd));
}

#[test]
fn cleanup_takes_back_a_created_worktree_and_its_branch() {
    let (_scratch, checkout, lifecycle) = fixture();
    let resolved = lifecycle
        .resolve(
            &WorktreeRequest::CreateNamed {
                name: "review".to_owned(),
                branch: None,
            },
            &checkout,
            None,
        )
        .expect("the worktree is raised");
    let prepared = resolved.prepared.expect("a created worktree");
    assert!(
        lifecycle
            .cleanup(Some(&prepared), resolved.pending_hold.as_ref())
            .is_none()
    );
    assert!(!prepared.path.exists());
    assert!(!git(
        &checkout,
        &["show-ref", "--verify", "--quiet", "refs/heads/review"]
    ));
}

#[test]
fn holding_outside_a_managed_worktree_only_releases_the_pending_hold() {
    let (_scratch, checkout, lifecycle) = fixture();
    assert!(lifecycle.hold_for_attachment(&checkout).is_none());
    lifecycle
        .hold(&checkout, "s1", None)
        .expect("an unmanaged directory is not an error");
    assert!(!lifecycle.is_managed(&checkout));
    assert!(!lifecycle.restore(&checkout).expect("nothing to restore"));
}
