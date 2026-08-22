//! What the corpus cannot reach.
//!
//! The differential replay beside this file compares this module against the
//! reference over scripted repositories, and it covers every shape that a
//! capture can script. What is here is what a capture cannot ask for, driven
//! through the same public functions the CLI calls.

use std::process::Command;

use super::{
    PreparedWorktree, WorktreeError, cleanup_failed_prepare, inspect_worktree_for_cleanup,
    prepare_worktree, remove_worktree,
};
use std::fs;
use std::path::{Path, PathBuf};

fn git(directory: &Path, arguments: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(arguments)
        .output()
        .expect("git is on PATH");
    assert!(
        output.status.success(),
        "git {arguments:?} failed in {}: {}",
        directory.display(),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn text(path: &Path) -> &str {
    path.to_str().expect("a scripted path is UTF-8")
}

#[cfg(unix)]
fn symlink(target: &Path, link: &Path) {
    std::os::unix::fs::symlink(target, link).expect("the fixture symlink is created");
}

#[cfg(windows)]
fn symlink(target: &Path, link: &Path) {
    std::os::windows::fs::symlink_dir(target, link).expect("the fixture symlink is created");
}

/// The managed root this port reaches for, so a test can assert on what a run
/// left there.
fn managed_directory(vibe_home: &Path, checkout: &Path) -> PathBuf {
    super::managed_worktree_root(vibe_home, checkout, &checkout.join(".git"))
}

/// A case root with no symbolic links in it, so `strip_prefix` against a
/// canonicalized checkout root behaves.
fn case_root() -> (tempfile::TempDir, PathBuf) {
    let scratch = tempfile::tempdir().expect("tempdir");
    let root = scratch
        .path()
        .canonicalize()
        .expect("the case root resolves");
    (scratch, root)
}

/// One committed checkout at `root/repo`, optionally keeping its git data
/// outside the working tree.
fn checkout(root: &Path, separate_git_dir: Option<&Path>) -> PathBuf {
    let checkout = root.join("repo");
    fs::create_dir_all(&checkout).expect("the checkout is writable");
    let mut arguments = vec!["init", "--quiet", "--initial-branch", "main"];
    let separate;
    if let Some(directory) = separate_git_dir {
        fs::create_dir_all(directory.parent().expect("the git data has a parent"))
            .expect("the git data directory is writable");
        separate = text(directory).to_owned();
        arguments.extend(["--separate-git-dir", separate.as_str()]);
    }
    git(&checkout, &arguments);
    git(&checkout, &["config", "user.name", "Vibe Test"]);
    git(&checkout, &["config", "user.email", "vibe@example.test"]);
    git(&checkout, &["config", "commit.gpgsign", "false"]);
    fs::write(checkout.join("README.md"), "fixture\n").expect("the fixture is writable");
    git(&checkout, &["add", "--all"]);
    git(
        &checkout,
        &["commit", "--quiet", "--no-gpg-sign", "-m", "fixture"],
    );
    checkout
}

/// A base inside the checkout that resolves to the checkout root itself but is
/// not spelled as it.
///
/// Preparation records the base relative to the checkout and reopens it inside
/// the new worktree, so a base that is an untracked symbolic link to the
/// checkout root exists for every git question asked before the worktree is
/// created and is missing from the worktree afterward. That is the only way to
/// make construction fail after `git worktree add` has already run, which is
/// exactly the span the rollback covers.
fn vanishing_base(checkout: &Path) -> PathBuf {
    let base = checkout.join("here");
    symlink(Path::new("."), &base);
    base
}

fn branch_is_present(checkout: &Path, branch: &str) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(checkout)
        .args([
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ])
        .output()
        .expect("git is on PATH")
        .status
        .success()
}

// --------------------------------------------------------------------------
// US-272: the commit count is taken against HEAD
// --------------------------------------------------------------------------

/// A worktree moved below where the session started discards nothing, and
/// saying so is not an error: `rev-list --count base..HEAD` answers zero when
/// `HEAD` is an ancestor of `base` rather than refusing the range.
#[test]
fn a_head_below_the_session_start_counts_no_commits() {
    let (_scratch, root) = case_root();
    let checkout = checkout(&root, None);
    let prepared =
        prepare_worktree("review", &checkout, &root.join("home")).expect("the worktree prepares");

    fs::write(prepared.root.join("note.txt"), "note\n").expect("the note is writable");
    git(&prepared.root, &["add", "--all"]);
    git(
        &prepared.root,
        &["commit", "--quiet", "--no-gpg-sign", "-m", "session"],
    );
    let ahead = PreparedWorktree {
        base_commit: git(&prepared.root, &["rev-parse", "HEAD"]),
        ..prepared
    };
    git(&ahead.root, &["reset", "--hard", "--quiet", "HEAD~1"]);

    let state = inspect_worktree_for_cleanup(&ahead).expect("the inspection answers");
    assert_eq!(state.new_commit_count, 0);
    assert!(state.is_clean());
}

/// A worktree whose `HEAD` git cannot resolve is a typed failure naming the
/// worktree, not a count of zero that would let removal proceed.
#[test]
fn an_unresolvable_head_fails_and_names_the_worktree() {
    let (_scratch, root) = case_root();
    let empty = root.join("empty");
    fs::create_dir_all(&empty).expect("the empty checkout is writable");
    git(&empty, &["init", "--quiet", "--initial-branch", "main"]);

    let worktree = PreparedWorktree {
        name: "review".to_owned(),
        branch: "review".to_owned(),
        root: empty.clone(),
        path: empty,
        repo_root: root.join("repo"),
        base_commit: "0".repeat(40),
        created: true,
        branch_created: true,
    };

    let error = inspect_worktree_for_cleanup(&worktree).expect_err("an unborn HEAD refuses");
    let WorktreeError::Failed { name, .. } = &error else {
        panic!("expected a named worktree failure, got {error:?}");
    };
    assert_eq!(name, "review");
}

// --------------------------------------------------------------------------
// US-273: a preparation that fails leaves nothing behind
// --------------------------------------------------------------------------

/// Construction failing after `git worktree add` undoes both halves of what the
/// call created.
#[test]
fn a_failed_construction_removes_the_worktree_and_the_branch_it_created() {
    let (_scratch, root) = case_root();
    let checkout = checkout(&root, None);
    let base = vanishing_base(&checkout);
    let vibe_home = root.join("home");

    let error = prepare_worktree("review", &base, &vibe_home).expect_err("construction fails");
    assert!(
        matches!(error, WorktreeError::Failed { .. }),
        "the original failure is returned unwrapped when the rollback finishes: {error:?}"
    );

    assert!(
        !managed_directory(&vibe_home, &checkout)
            .join("review")
            .exists(),
        "the worktree the failed run created is still under the managed root"
    );
    assert!(
        !git(&checkout, &["worktree", "list", "--porcelain"]).contains("review"),
        "git still records the worktree the failed run created"
    );
    assert!(
        !branch_is_present(&checkout, "review"),
        "the branch the failed run created survived it"
    );
}

/// A branch that existed before the run is not this run's to delete.
#[test]
fn a_failed_construction_keeps_a_branch_it_did_not_create() {
    let (_scratch, root) = case_root();
    let checkout = checkout(&root, None);
    git(&checkout, &["branch", "review"]);
    let base = vanishing_base(&checkout);

    prepare_worktree("review", &base, &root.join("home")).expect_err("construction fails");

    assert!(
        !git(&checkout, &["worktree", "list", "--porcelain"]).contains("review"),
        "git still records the worktree the failed run created"
    );
    assert!(
        branch_is_present(&checkout, "review"),
        "a branch that predates the run was deleted by its rollback"
    );
}

/// A rollback that cannot finish is attached to the failure it was undoing,
/// and the pair reaches the caller with both halves readable.
#[test]
fn a_failing_rollback_is_attached_to_the_original_failure() {
    let (_scratch, root) = case_root();
    let checkout = checkout(&root, None);
    let absent = root.join("never-added");

    let note = cleanup_failed_prepare(&checkout, &absent, "review", true)
        .expect("removing a worktree git never recorded fails");

    let noted = WorktreeError::Noted {
        name: "review".to_owned(),
        source: Box::new(WorktreeError::failed("review", "construction failed")),
        note,
    };
    let rendered = noted.to_string();
    assert!(
        rendered.contains("construction failed"),
        "the original failure was swallowed: {rendered}"
    );
    assert!(
        rendered.contains("could not be removed"),
        "the rollback failure was swallowed: {rendered}"
    );
}

/// A failure raised before `git worktree add` ran has nothing to undo, so the
/// rollback is never entered and the managed root stays empty.
#[test]
fn a_failure_before_the_checkout_removes_nothing() {
    let (_scratch, root) = case_root();
    let checkout = checkout(&root, None);
    let vibe_home = root.join("home");
    let occupied = managed_directory(&vibe_home, &checkout).join("review");
    fs::create_dir_all(&occupied).expect("the occupying directory is writable");
    fs::write(occupied.join("note.txt"), "occupied\n").expect("the note is writable");

    prepare_worktree("review", &checkout, &vibe_home).expect_err("an occupied target refuses");

    assert!(
        occupied.join("note.txt").is_file(),
        "a directory this run did not create was removed by it"
    );
    assert!(!branch_is_present(&checkout, "review"));
}

// --------------------------------------------------------------------------
// US-274: the primary checkout
// --------------------------------------------------------------------------

/// A repository keeping its git data outside the working tree can prepare a
/// worktree and remove it again, which needs the repository root to be the
/// primary checkout and not the directory beside the git data.
#[test]
fn a_separate_git_directory_repository_prepares_and_removes() {
    let (_scratch, root) = case_root();
    let checkout = checkout(&root, Some(&root.join("state").join("repo.git")));

    let prepared =
        prepare_worktree("review", &checkout, &root.join("home")).expect("the worktree prepares");
    assert_eq!(prepared.repo_root, checkout);

    remove_worktree(&prepared, true).expect("the worktree is removable");
    assert!(!prepared.root.exists());
    assert!(!branch_is_present(&checkout, "review"));
}

/// A linked worktree of such a repository has no primary checkout to name, and
/// refusing is what keeps a later `worktree remove` from running in a directory
/// that is not a checkout.
#[test]
fn a_linked_worktree_of_a_separate_git_directory_repository_refuses() {
    let (_scratch, root) = case_root();
    let checkout = checkout(&root, Some(&root.join("state").join("repo.git")));
    let linked = root.join("linked").join("alpha");
    fs::create_dir_all(linked.parent().expect("the linked directory has a parent"))
        .expect("the linked directory is writable");
    git(
        &checkout,
        &["worktree", "add", "--quiet", "-b", "alpha", text(&linked)],
    );
    let vibe_home = root.join("home");

    let error = prepare_worktree("review", &linked, &vibe_home)
        .expect_err("the primary checkout is unknown");
    let WorktreeError::Failed { name, message } = &error else {
        panic!("expected a named worktree failure, got {error:?}");
    };
    assert_eq!(name, "review");
    assert!(message.contains("primary checkout"), "{message}");
    assert!(!vibe_home.join("worktrees").exists());
    assert!(!branch_is_present(&checkout, "review"));
}

/// Reusing a worktree reached through a symbolic link finds the same
/// repository, because both sides of the comparison are resolved first.
#[test]
fn a_symlinked_managed_root_still_reads_as_the_same_repository() {
    let (_scratch, root) = case_root();
    let checkout = checkout(&root, None);
    let vibe_home = root.join("home");
    let first = prepare_worktree("review", &checkout, &vibe_home).expect("the worktree prepares");
    assert!(first.created);

    let alias = root.join("alias");
    symlink(&vibe_home, &alias);
    let second = prepare_worktree("review", &checkout, &alias).expect("the worktree is reused");

    assert!(
        !second.created,
        "a symlinked path read as a different repository"
    );
    assert!(!second.branch_created);
}
