use std::process::Command;

use tempfile::tempdir;

use super::*;

pub(in crate::projects) fn run_test_git(working_directory: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(working_directory)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .status()
        .expect("Git test command starts");
    assert!(status.success(), "Git test command failed: {args:?}");
}

pub(in crate::projects) fn committed_github_repository() -> tempfile::TempDir {
    let repository = tempdir().expect("temporary Git repository");
    run_test_git(repository.path(), &["init", "--quiet"]);
    run_test_git(repository.path(), &["config", "user.name", "Vibe Test"]);
    run_test_git(
        repository.path(),
        &["config", "user.email", "vibe@example.test"],
    );
    run_test_git(repository.path(), &["branch", "-M", "main"]);
    fs::write(repository.path().join("tracked.txt"), "base\n").expect("tracked fixture");
    run_test_git(repository.path(), &["add", "--", "tracked.txt"]);
    run_test_git(repository.path(), &["commit", "--quiet", "-m", "base"]);
    run_test_git(
        repository.path(),
        &["remote", "add", "origin", "git@github.com:owner/repo.git"],
    );
    run_test_git(
        repository.path(),
        &["update-ref", "refs/remotes/origin/main", "HEAD"],
    );
    repository
}

#[test]
fn command_git_probe_tolerates_fetch_failure_and_reports_dirty_changes() {
    let repository = committed_github_repository();
    let nested = repository.path().join("nested/deeper");
    fs::create_dir_all(&nested).expect("nested working directory");
    fs::write(repository.path().join("tracked.txt"), "changed\n").expect("tracked change");
    fs::write(
        repository.path().join("untracked.bin"),
        [0_u8, 1, 2, 0xff, 0, 0x80, 3],
    )
    .expect("untracked change");
    let probe =
        CommandGitProbe::default().with_timeouts(Duration::from_secs(2), Duration::from_millis(1));

    let snapshot = probe.inspection(&nested).expect("Git inspection succeeds");

    assert!(snapshot.dirty);
    assert!(!snapshot.unpushed);
    assert_eq!(snapshot.repository, "https://github.com/owner/repo.git");
    assert!(!repository.path().join(".git/index.lock").exists());
}

#[test]
fn command_git_probe_reports_unpushed_commits() {
    let repository = committed_github_repository();
    for (name, contents) in [("one.txt", "one\n"), ("two.txt", "two\n")] {
        fs::write(repository.path().join(name), contents).expect("commit fixture");
        run_test_git(repository.path(), &["add", "--", name]);
        run_test_git(repository.path(), &["commit", "--quiet", "-m", name]);
    }
    let probe =
        CommandGitProbe::default().with_timeouts(Duration::from_secs(2), Duration::from_millis(1));

    let snapshot = probe
        .inspection(repository.path())
        .expect("Git inspection succeeds");

    assert!(snapshot.unpushed);
    assert!(!snapshot.dirty);
}

#[test]
fn git_remote_selection_prefers_an_eligible_github_remote_and_rejects_paths() {
    let repository = committed_github_repository();
    run_test_git(repository.path(), &["remote", "remove", "origin"]);
    run_test_git(
        repository.path(),
        &[
            "remote",
            "add",
            "origin",
            "https://gitlab.example/owner/repo.git",
        ],
    );
    run_test_git(
        repository.path(),
        &[
            "remote",
            "add",
            "github",
            "ssh://git@github.com/owner/repo.git",
        ],
    );
    let metadata = CommandGitProbe::default()
        .metadata(repository.path())
        .expect("eligible remote");
    assert_eq!(metadata.remote, "github");
    assert_eq!(metadata.repo_url, "https://github.com/owner/repo.git");

    for value in [
        "C:\\workspace\\repo",
        "\\\\server\\share\\repo",
        "/workspace/repo",
        "../repo",
        "file:///workspace/repo",
    ] {
        assert!(matches!(
            sanitize_git_remote(value),
            Err(CloudError::Git(_))
        ));
    }
}
