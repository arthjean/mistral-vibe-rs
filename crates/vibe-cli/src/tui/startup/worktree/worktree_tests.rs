use std::io::Cursor;
use std::process::Command;

use std::fs;

use super::*;

fn test_arguments(root: &Path) -> Arguments {
    let mut arguments = crate::arguments_for_test();
    arguments.workdir = Some(root.to_path_buf());
    arguments.session_root = Some(root.join("vibe-home/sessions"));
    arguments
}

fn git(directory: &Path, arguments: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(arguments)
        .status()
        .expect("git runs");
    assert!(status.success(), "git {arguments:?} failed");
}

fn git_stdout(directory: &Path, arguments: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(arguments)
        .output()
        .expect("git runs");
    assert!(output.status.success(), "git {arguments:?} failed");
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn git_succeeds(directory: &Path, arguments: &[&str]) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(arguments)
        .output()
        .expect("git runs")
        .status
        .success()
}

fn path_text(path: &Path) -> &str {
    path.to_str().expect("path is valid UTF-8")
}

fn repository() -> tempfile::TempDir {
    let root = tempfile::tempdir().expect("repository");
    git(root.path(), &["init", "-q"]);
    git(root.path(), &["config", "user.name", "Vibe Test"]);
    git(root.path(), &["config", "user.email", "vibe@example.test"]);
    fs::write(root.path().join("README.md"), "fixture\n").expect("fixture");
    git(root.path(), &["add", "README.md"]);
    git(root.path(), &["commit", "-qm", "fixture"]);
    root
}

#[test]
fn worktree_is_prepared_at_the_reference_location_and_reused() {
    let root = repository();
    let mut arguments = test_arguments(root.path());
    arguments.worktree = Some(Some("parity".to_owned()));
    let first =
        LaunchWorkspace::prepare(&mut arguments, &mut Vec::<u8>::new()).expect("worktree prepared");
    let prepared = first.worktree.expect("prepared worktree");
    assert!(prepared.created);
    assert!(prepared.path.join("README.md").is_file());
    assert_eq!(arguments.workdir.as_deref(), Some(prepared.path.as_path()));
    assert!(arguments.trust);

    let mut reused_arguments = test_arguments(root.path());
    reused_arguments.worktree = Some(Some("parity".to_owned()));
    let reused = LaunchWorkspace::prepare(&mut reused_arguments, &mut Vec::<u8>::new())
        .expect("existing worktree reused")
        .worktree
        .expect("reused worktree");
    assert!(!reused.created);
    assert_eq!(reused.path, prepared.path);

    git(
        root.path(),
        &["worktree", "remove", "--force", path_text(&prepared.root)],
    );
    git(root.path(), &["branch", "-D", "parity"]);
}

/// An empty `--worktree` names no worktree, so the launch runs where it
/// stood instead of failing on an unportable name.
#[test]
fn an_empty_worktree_flag_starts_a_normal_session() {
    let root = repository();
    let mut arguments = test_arguments(root.path());
    arguments.worktree = Some(Some(String::new()));

    let workspace = LaunchWorkspace::prepare(&mut arguments, &mut Vec::<u8>::new())
        .expect("a normal session starts");

    assert!(workspace.worktree.is_none());
    assert_eq!(
        workspace.effective_directory,
        vibe_core::platform::canonicalize(root.path()).expect("the fixture root resolves")
    );
    assert!(!arguments.trust, "an empty flag granted trust");
}

/// `VIBE_HOME` is read as a path the shell never expanded, so the tilde is
/// expanded here before it names a directory.
#[test]
fn a_tilde_in_a_home_path_expands() {
    let Some(home) = vibe_core::platform::user_home() else {
        return;
    };
    assert_eq!(
        vibe_core::platform::expand_home(Path::new("~/.vibe")),
        home.join(".vibe")
    );
}

#[test]
fn invalid_and_non_repository_worktrees_fail_before_checkout_changes() {
    let root = repository();
    let original_head = git_stdout(root.path(), &["rev-parse", "HEAD"]);
    let mut invalid = test_arguments(root.path());
    invalid.worktree = Some(Some("../escape".to_owned()));
    assert!(matches!(
        LaunchWorkspace::prepare(&mut invalid, &mut Vec::<u8>::new()),
        Err(StartupError::InvalidWorktreeName)
    ));
    assert_eq!(
        git_stdout(root.path(), &["rev-parse", "HEAD"]),
        original_head
    );

    let outside = tempfile::tempdir().expect("non-repository");
    let mut non_repository = test_arguments(outside.path());
    non_repository.worktree = Some(Some("feature".to_owned()));
    assert!(matches!(
        LaunchWorkspace::prepare(&mut non_repository, &mut Vec::<u8>::new()),
        Err(StartupError::WorktreeRepositoryRequired)
    ));
}

#[test]
fn conflicting_worktree_path_is_preserved() {
    let root = repository();
    let mut arguments = test_arguments(root.path());
    arguments.worktree = Some(Some("conflict".to_owned()));
    let prepared = LaunchWorkspace::prepare(&mut arguments, &mut Vec::<u8>::new())
        .expect("initial worktree")
        .worktree
        .expect("prepared worktree");
    let target = prepared.root.clone();
    cleanup_worktree(
        prepared,
        &mut Cursor::new(Vec::<u8>::new()),
        &mut Vec::new(),
    )
    .expect("initial worktree removed");
    fs::create_dir_all(&target).expect("conflicting directory");
    fs::write(target.join("owner.txt"), "preserve me\n").expect("conflicting content");

    let mut conflicting = test_arguments(root.path());
    conflicting.worktree = Some(Some("conflict".to_owned()));
    assert!(matches!(
        LaunchWorkspace::prepare(&mut conflicting, &mut Vec::<u8>::new()),
        Err(StartupError::Worktree { .. })
    ));
    assert_eq!(
        fs::read_to_string(target.join("owner.txt")).expect("conflict preserved"),
        "preserve me\n"
    );
}

#[test]
fn attached_preexisting_branch_can_survive_cleanup() {
    let root = repository();
    git(root.path(), &["branch", "attached"]);
    let mut arguments = test_arguments(root.path());
    arguments.worktree = Some(Some("attached".to_owned()));
    let prepared = LaunchWorkspace::prepare(&mut arguments, &mut Vec::<u8>::new())
        .expect("attached branch worktree")
        .worktree
        .expect("prepared worktree");
    let worktree_root = prepared.root.clone();
    let outcome = cleanup_worktree(prepared, &mut Cursor::new(b"n\n"), &mut Vec::new())
        .expect("cleanup choice");
    assert_eq!(outcome, CleanupOutcome::Removed);
    assert!(!worktree_root.exists());
    assert!(git_succeeds(
        root.path(),
        &["show-ref", "--verify", "--quiet", "refs/heads/attached"]
    ));
    git(root.path(), &["branch", "-D", "attached"]);
}

#[test]
fn relative_additional_directory_is_resolved_inside_the_worktree() {
    let root = repository();
    fs::create_dir_all(root.path().join("nested/context")).expect("nested directory");
    fs::write(root.path().join("nested/context/file.txt"), "context\n").expect("context file");
    git(root.path(), &["add", "nested/context/file.txt"]);
    git(root.path(), &["commit", "-qm", "nested fixture"]);
    let mut arguments = test_arguments(root.path());
    arguments.worktree = Some(Some("relative-add-dir".to_owned()));
    arguments.add_directories = vec![PathBuf::from("nested/context")];
    let prepared = LaunchWorkspace::prepare(&mut arguments, &mut Vec::<u8>::new())
        .expect("worktree prepared")
        .worktree
        .expect("prepared worktree");
    assert_eq!(
        arguments.add_directories,
        vec![prepared.path.join("nested/context")]
    );
    cleanup_worktree(
        prepared,
        &mut Cursor::new(Vec::<u8>::new()),
        &mut Vec::new(),
    )
    .expect("clean worktree removed");
}

#[test]
fn dirty_owned_worktree_is_kept_when_cleanup_is_declined() {
    let root = repository();
    let mut arguments = test_arguments(root.path());
    arguments.worktree = Some(Some("dirty".to_owned()));
    let prepared = LaunchWorkspace::prepare(&mut arguments, &mut Vec::<u8>::new())
        .expect("worktree prepared")
        .worktree
        .expect("prepared worktree");
    fs::write(prepared.path.join("dirty.txt"), "dirty\n").expect("dirty file");
    let worktree_root = prepared.root.clone();
    let outcome = cleanup_worktree(prepared, &mut Cursor::new(b"n\n"), &mut Vec::new())
        .expect("cleanup decision");
    assert_eq!(outcome, CleanupOutcome::Kept);
    assert!(worktree_root.is_dir());
    git(
        root.path(),
        &["worktree", "remove", "--force", path_text(&worktree_root)],
    );
    git(root.path(), &["branch", "-D", "dirty"]);
}

/// A commit made while `HEAD` is detached never moves the branch tip, so
/// counting against the branch would report a clean worktree and remove it
/// without asking. Declining has to be reachable, which means the prompt
/// has to happen.
#[test]
fn detached_head_commit_is_kept_when_cleanup_is_declined() {
    let root = repository();
    let mut arguments = test_arguments(root.path());
    arguments.worktree = Some(Some("detached".to_owned()));
    let prepared = LaunchWorkspace::prepare(&mut arguments, &mut Vec::<u8>::new())
        .expect("worktree prepared")
        .worktree
        .expect("prepared worktree");
    let worktree_root = prepared.root.clone();
    git(&worktree_root, &["checkout", "--quiet", "--detach"]);
    fs::write(worktree_root.join("note.txt"), "note\n").expect("note");
    git(&worktree_root, &["add", "--all"]);
    git(&worktree_root, &["commit", "-qm", "detached work"]);

    let state = inspect_worktree_for_cleanup(&prepared).expect("inspection");
    assert_eq!(state.new_commit_count, 1);
    assert!(!state.is_clean());

    let outcome = cleanup_worktree(prepared, &mut Cursor::new(b"n\n"), &mut Vec::new())
        .expect("cleanup decision");
    assert_eq!(outcome, CleanupOutcome::Kept);
    assert!(worktree_root.is_dir());
    assert!(git_succeeds(
        root.path(),
        &["show-ref", "--verify", "--quiet", "refs/heads/detached"]
    ));

    git(
        root.path(),
        &["worktree", "remove", "--force", path_text(&worktree_root)],
    );
    git(root.path(), &["branch", "-D", "detached"]);
}

/// Both narration lines are the reference's, in its order: the requested
/// name before git runs, the resolved path once it did.
#[test]
fn preparation_narrates_the_name_then_the_path() {
    let root = repository();
    let mut arguments = test_arguments(root.path());
    arguments.worktree = Some(Some("narrated".to_owned()));
    let mut narration = Vec::<u8>::new();
    let prepared = LaunchWorkspace::prepare(&mut arguments, &mut narration)
        .expect("worktree prepared")
        .worktree
        .expect("prepared worktree");
    let narration = String::from_utf8(narration).expect("narration is UTF-8");
    let preparing = narration
        .find("Preparing worktree 'narrated'...")
        .expect("the name is narrated");
    let using = narration
        .find(&format!("Using worktree: {}", prepared.path.display()))
        .expect("the path is narrated");
    assert!(preparing < using, "narration out of order: {narration}");

    cleanup_worktree(
        prepared,
        &mut Cursor::new(Vec::<u8>::new()),
        &mut Vec::new(),
    )
    .expect("clean worktree removed");
}

/// A preparation that fails narrates the attempt and nothing after it, so
/// the failure the caller reports is not preceded by a path that never
/// existed.
#[test]
fn a_failed_preparation_narrates_only_the_attempt() {
    let outside = tempfile::tempdir().expect("non-repository");
    let mut arguments = test_arguments(outside.path());
    arguments.worktree = Some(Some("doomed".to_owned()));
    let mut narration = Vec::<u8>::new();
    assert!(LaunchWorkspace::prepare(&mut arguments, &mut narration).is_err());
    let narration = String::from_utf8(narration).expect("narration is UTF-8");
    assert!(narration.contains("Preparing worktree 'doomed'..."));
    assert!(!narration.contains("Using worktree:"), "{narration}");
}

/// The removal is announced before it is attempted, so a git failure has a
/// line naming what was being removed when it happened.
#[test]
fn cleanup_names_the_worktree_before_removing_it() {
    let root = repository();
    let mut arguments = test_arguments(root.path());
    arguments.worktree = Some(Some("announced".to_owned()));
    let prepared = LaunchWorkspace::prepare(&mut arguments, &mut Vec::<u8>::new())
        .expect("worktree prepared")
        .worktree
        .expect("prepared worktree");
    let worktree_root = prepared.root.clone();
    let mut output = Vec::<u8>::new();
    let outcome = cleanup_worktree(prepared, &mut Cursor::new(Vec::<u8>::new()), &mut output)
        .expect("clean worktree removed");
    assert_eq!(outcome, CleanupOutcome::Removed);
    let output = String::from_utf8(output).expect("output is UTF-8");
    let removing = output
        .find(&format!("Removing worktree: {}", worktree_root.display()))
        .expect("the removal is announced");
    let removed = output
        .find(&format!("Removed worktree: {}", worktree_root.display()))
        .expect("the removal is confirmed");
    assert!(removing < removed, "cleanup out of order: {output}");
}

/// A worktree git can no longer describe cannot be the subject of a
/// truthful question, so cleanup says so and keeps whatever is left.
#[test]
fn an_uninspectable_worktree_is_named_and_kept() {
    let root = repository();
    let mut arguments = test_arguments(root.path());
    arguments.worktree = Some(Some("uninspectable".to_owned()));
    let prepared = LaunchWorkspace::prepare(&mut arguments, &mut Vec::<u8>::new())
        .expect("worktree prepared")
        .worktree
        .expect("prepared worktree");
    let worktree_root = prepared.root.clone();
    fs::remove_dir_all(&worktree_root).expect("worktree directory removed underneath");

    let mut output = Vec::<u8>::new();
    let outcome = cleanup_worktree(prepared, &mut Cursor::new(Vec::<u8>::new()), &mut output)
        .expect("an inspection failure is not fatal");
    assert_eq!(outcome, CleanupOutcome::Kept);
    let output = String::from_utf8(output).expect("output is UTF-8");
    assert!(
        output.contains("Could not inspect worktree for cleanup:"),
        "{output}"
    );
    assert!(!output.contains("Removing worktree:"), "{output}");

    git(root.path(), &["worktree", "prune"]);
    git(root.path(), &["branch", "-D", "uninspectable"]);
}

/// A removal git refuses is reported and ends the cleanup there: no
/// confirmation line follows it and whatever git left on disk stays.
#[test]
fn a_removal_git_refuses_is_reported_and_stops_the_cleanup() {
    let root = repository();
    let mut arguments = test_arguments(root.path());
    arguments.worktree = Some(Some("refused".to_owned()));
    let prepared = LaunchWorkspace::prepare(&mut arguments, &mut Vec::<u8>::new())
        .expect("worktree prepared")
        .worktree
        .expect("prepared worktree");
    let worktree_root = prepared.root.clone();
    // Inspection reads the worktree itself and still answers; the removal
    // runs against the checkout, and a checkout git cannot read refuses it.
    let elsewhere = tempfile::tempdir().expect("non-repository");
    let refused = PreparedWorktree {
        repo_root: elsewhere.path().to_path_buf(),
        ..prepared
    };

    let mut output = Vec::<u8>::new();
    let outcome = cleanup_worktree(refused, &mut Cursor::new(Vec::<u8>::new()), &mut output)
        .expect("a refused removal is not fatal");
    assert_eq!(outcome, CleanupOutcome::Failed);
    let output = String::from_utf8(output).expect("output is UTF-8");
    assert!(
        output.contains(&format!("Removing worktree: {}", worktree_root.display())),
        "{output}"
    );
    assert!(output.contains("Could not remove worktree:"), "{output}");
    assert!(!output.contains("Removed worktree:"), "{output}");
    assert!(
        worktree_root.is_dir(),
        "a worktree git refused to remove is still on disk"
    );

    git(
        root.path(),
        &["worktree", "remove", "--force", path_text(&worktree_root)],
    );
    git(root.path(), &["branch", "-D", "refused"]);
}

/// The reference asks through `input()`, which raises at end of file and is
/// caught as a refusal, so a closed stdin keeps the work.
#[test]
fn end_of_input_declines_and_keeps_the_worktree() {
    let root = repository();
    let mut arguments = test_arguments(root.path());
    arguments.worktree = Some(Some("eof".to_owned()));
    let prepared = LaunchWorkspace::prepare(&mut arguments, &mut Vec::<u8>::new())
        .expect("worktree prepared")
        .worktree
        .expect("prepared worktree");
    fs::write(prepared.path.join("unsaved.txt"), "unsaved\n").expect("dirty file");
    let worktree_root = prepared.root.clone();

    let mut output = Vec::<u8>::new();
    let outcome = cleanup_worktree(prepared, &mut Cursor::new(Vec::<u8>::new()), &mut output)
        .expect("end of input is a decision, not a failure");
    assert_eq!(outcome, CleanupOutcome::Kept);
    assert!(worktree_root.is_dir());
    let output = String::from_utf8(output).expect("output is UTF-8");
    assert!(
        output.contains(&format!("Keeping worktree: {}", worktree_root.display())),
        "{output}"
    );

    git(
        root.path(),
        &["worktree", "remove", "--force", path_text(&worktree_root)],
    );
    git(root.path(), &["branch", "-D", "eof"]);
}

/// The gate is the reference's: this run's own worktree, no `--prompt`, and
/// an exit code of zero or none.
#[test]
fn cleanup_is_offered_only_for_an_owned_worktree_of_a_run_that_ended_well() {
    let owned = PreparedWorktree {
        name: "owned".to_owned(),
        path: PathBuf::from("/managed/owned"),
        root: PathBuf::from("/managed/owned"),
        repo_root: PathBuf::from("/checkout"),
        base_commit: "0".repeat(40),
        branch: "owned".to_owned(),
        created: true,
        branch_created: true,
        pending_hold: None,
    };
    let reused = PreparedWorktree {
        created: false,
        ..owned.clone()
    };

    assert!(cleanup_is_offered(Some(&owned), None, Some(0)));
    assert!(cleanup_is_offered(Some(&owned), None, None));
    assert!(!cleanup_is_offered(Some(&owned), None, Some(1)));
    assert!(!cleanup_is_offered(
        Some(&owned),
        Some("do the thing"),
        None
    ));
    assert!(!cleanup_is_offered(Some(&reused), None, Some(0)));
    assert!(!cleanup_is_offered(None, None, Some(0)));
}

#[test]
fn clean_owned_worktree_is_removed_once() {
    let root = repository();
    let mut arguments = test_arguments(root.path());
    arguments.worktree = Some(Some("clean".to_owned()));
    let prepared = LaunchWorkspace::prepare(&mut arguments, &mut Vec::<u8>::new())
        .expect("worktree prepared")
        .worktree
        .expect("prepared worktree");
    let path = prepared.root.clone();
    let outcome = cleanup_worktree(
        prepared,
        &mut Cursor::new(Vec::<u8>::new()),
        &mut Vec::new(),
    )
    .expect("clean worktree removed");
    assert_eq!(outcome, CleanupOutcome::Removed);
    assert!(!path.exists());
}

/// A bare flag names the worktree itself: with no prompt to name it after,
/// a random slug on a `vibe/` branch, narrated without a name.
#[test]
fn a_bare_flag_prepares_an_unnamed_worktree_on_a_vibe_branch() {
    let root = repository();
    let mut arguments = test_arguments(root.path());
    arguments.worktree = Some(None);
    let mut narration = Vec::<u8>::new();
    let prepared = LaunchWorkspace::prepare(&mut arguments, &mut narration)
        .expect("worktree prepared")
        .worktree
        .expect("prepared worktree");
    let narration = String::from_utf8(narration).expect("narration is UTF-8");

    assert!(
        narration.starts_with("Preparing worktree...\n"),
        "{narration}"
    );
    assert!(prepared.created && prepared.branch_created);
    assert_eq!(prepared.branch, format!("vibe/{}", prepared.name));
    assert!(vibe_core::worktree::is_portable_worktree_name(
        &prepared.name
    ));

    git(
        root.path(),
        &["worktree", "remove", "--force", path_text(&prepared.root)],
    );
    git(root.path(), &["branch", "-D", &prepared.branch]);
}

/// The launch stands in its worktree as `cli-<pid>` until it lets go, which
/// is what keeps an app server sweeping the repository off it.
#[test]
fn the_launch_holds_its_worktree_until_released() {
    let root = repository();
    let mut arguments = test_arguments(root.path());
    arguments.worktree = Some(Some("held".to_owned()));
    let workspace =
        LaunchWorkspace::prepare(&mut arguments, &mut Vec::<u8>::new()).expect("worktree prepared");
    let prepared = workspace.worktree.clone().expect("prepared worktree");
    let managed = ManagedRoot::containing(&prepared.root)
        .and_then(|managed| ManagedWorktree::at(&managed, &prepared.root))
        .expect("the worktree is managed");

    assert!(managed.holders().contains(&cli_worktree_holder()));
    workspace.release_holder();
    assert!(managed.holders().is_empty());

    git(
        root.path(),
        &["worktree", "remove", "--force", path_text(&prepared.root)],
    );
    git(root.path(), &["branch", "-D", "held"]);
}

/// A session elsewhere standing in the worktree keeps it: removing it would
/// leave that session in a deleted directory.
#[test]
fn cleanup_keeps_a_worktree_another_session_holds() {
    let root = repository();
    let mut arguments = test_arguments(root.path());
    arguments.worktree = Some(Some("shared".to_owned()));
    let workspace =
        LaunchWorkspace::prepare(&mut arguments, &mut Vec::<u8>::new()).expect("worktree prepared");
    let prepared = workspace.worktree.clone().expect("prepared worktree");
    let managed = ManagedRoot::containing(&prepared.root)
        .and_then(|managed| ManagedWorktree::at(&managed, &prepared.root))
        .expect("the worktree is managed");
    managed.hold("other-session", None).expect("second holder");
    let worktree_root = prepared.root.clone();

    let mut output = Vec::<u8>::new();
    let outcome = cleanup_worktree(prepared, &mut Cursor::new(Vec::<u8>::new()), &mut output)
        .expect("a held worktree is kept");
    assert_eq!(outcome, CleanupOutcome::Kept);
    let output = String::from_utf8(output).expect("output is UTF-8");
    assert_eq!(
        output,
        format!(
            "Keeping worktree {}: in use by 1 other session(s)\n",
            worktree_root.display()
        )
    );
    assert!(worktree_root.is_dir());

    managed.release_holder("other-session").expect("released");
    workspace.release_holder();
    git(
        root.path(),
        &["worktree", "remove", "--force", path_text(&worktree_root)],
    );
    git(root.path(), &["branch", "-D", "shared"]);
}
