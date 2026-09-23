use std::fs;
use std::path::Path;

use super::*;

fn repository(root: &Path, name: &str, config: &str) -> PathBuf {
    let directory = root.join(name);
    fs::create_dir_all(directory.join(".git")).expect("git directory");
    fs::write(directory.join(".git").join("config"), config).expect("config");
    directory
}

fn asks(directory: &Path, subcommand: &str) -> bool {
    git_repository_requires_approval(&["git".to_owned(), subcommand.to_owned()], directory)
}

/// Each configuration answers `status`, `diff` and `log` as the pinned
/// reference answered the same fixture.
#[test]
fn a_repository_setting_that_runs_a_helper_asks() {
    let root = tempfile::tempdir().expect("root");
    for (name, config, expected) in [
        ("plain", "[core]\n\tbare = false\n", [false, false, false]),
        ("pager", "[core]\n\tpager = less\n", [true, true, true]),
        (
            "pager-off",
            "[core]\n\tpager = off\n",
            [false, false, false],
        ),
        ("pager-empty", "[core]\n\tpager =\n", [true, true, true]),
        ("include", "[include]\n\tpath = x\n", [true, true, true]),
        ("log-pager", "[pager]\n\tlog = cat\n", [false, false, true]),
        ("fsmonitor", "[core]\n\tfsmonitor\n", [true, true, false]),
        (
            "diff-driver",
            "[diff \"x\"]\n\ttextconv = cat\n",
            [true, true, true],
        ),
        ("gpg", "[gpg]\n\tprogram = gpg2\n", [false, false, true]),
    ] {
        let directory = repository(root.path(), name, config);
        let answered = ["status", "diff", "log"].map(|subcommand| asks(&directory, subcommand));
        assert_eq!(answered, expected, "`{name}`");
    }
}

/// A linked worktree is named by its own git directory and reads its
/// `config.worktree` beside the shared configuration.
#[test]
fn a_linked_worktree_keeps_its_own_identity_and_configuration() {
    let root = tempfile::tempdir().expect("root");
    let main = repository(root.path(), "main", "[core]\n\tbare = false\n");
    let git_dir = main.join(".git").join("worktrees").join("linked");
    fs::create_dir_all(&git_dir).expect("worktree git directory");
    fs::write(git_dir.join("commondir"), "../..\n").expect("commondir");
    fs::write(git_dir.join("config.worktree"), "[core]\n\tpager = less\n")
        .expect("worktree config");
    let linked = root.path().join("linked");
    fs::create_dir_all(&linked).expect("linked checkout");
    fs::write(
        linked.join(".git"),
        "gitdir: ../main/.git/worktrees/linked\n",
    )
    .expect("gitfile");

    assert!(asks(&linked, "status"));
    assert!(!asks(&main, "status"));
    let canonical = |path: &Path| {
        fs::canonicalize(path)
            .expect("canonical")
            .display()
            .to_string()
    };
    assert_eq!(git_repository_identity(&linked), Some(canonical(&git_dir)));
    assert_eq!(
        git_repository_identity(&main),
        Some(canonical(&main.join(".git")))
    );
}

#[test]
fn a_directory_outside_any_repository_has_no_identity() {
    assert_eq!(git_repository_identity(Path::new("/")), None);
}

#[test]
fn lines_split_as_python_splits_them() {
    assert_eq!(
        split_lines("a\r\nb\rc\n\nd\n"),
        vec!["a", "b", "c", "", "d"]
    );
    assert!(split_lines("").is_empty());
}
