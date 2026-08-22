//! What the corpus cannot reach.
//!
//! The differential replay beside this file compares this module against the
//! reference over scripted repositories, and it covers every shape that a
//! capture can script. What is here is what a capture cannot ask for, driven
//! through the same public functions the CLI calls.

use std::process::Command;

use super::{PreparedWorktree, WorktreeError, inspect_worktree_for_cleanup, prepare_worktree};
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
