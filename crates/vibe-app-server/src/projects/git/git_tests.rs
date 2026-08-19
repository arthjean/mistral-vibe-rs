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
fn command_git_probe_tolerates_fetch_failure_and_transfers_dirty_only_changes() {
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

    let (snapshot, context, push) = probe.inspection(&nested).expect("Git inspection succeeds");

    assert!(snapshot.dirty);
    assert!(!snapshot.unpushed);
    assert_eq!(push.unpushed_count, 0);
    assert!(!push.branch_not_pushed);
    let encoded = context.diff.expect("dirty diff");
    let compressed = BASE64_STANDARD
        .decode(encoded.content)
        .expect("base64 diff");
    let decoded = zstd::stream::decode_all(compressed.as_slice()).expect("zstd diff");
    let decoded = String::from_utf8(decoded).expect("UTF-8 Git patch");
    assert!(decoded.contains("changed"));
    assert!(decoded.contains("untracked.bin"));
    assert!(decoded.contains("GIT binary patch"));
    assert!(!repository.path().join(".git/index.lock").exists());
}

#[test]
fn command_git_probe_reports_true_unpushed_commit_count() {
    let repository = committed_github_repository();
    for (name, contents) in [("one.txt", "one\n"), ("two.txt", "two\n")] {
        fs::write(repository.path().join(name), contents).expect("commit fixture");
        run_test_git(repository.path(), &["add", "--", name]);
        run_test_git(repository.path(), &["commit", "--quiet", "-m", name]);
    }
    let probe =
        CommandGitProbe::default().with_timeouts(Duration::from_secs(2), Duration::from_millis(1));

    let (snapshot, context, push) = probe
        .inspection(repository.path())
        .expect("Git inspection succeeds");

    assert!(snapshot.unpushed);
    assert!(!snapshot.dirty);
    assert_eq!(push.unpushed_count, 2);
    assert!(!push.branch_not_pushed);
    assert!(context.diff.is_none());
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

#[test]
fn oversized_encoded_diff_fails_instead_of_truncating() {
    let mut state = 0x1234_5678_u32;
    let mut diff = vec![0_u8; 800_000];
    for byte in &mut diff {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        *byte = state as u8;
    }
    assert!(matches!(
        encode_working_tree_diff(&diff),
        Err(CloudError::Git(message)) if message.contains("Teleport limit")
    ));
}
