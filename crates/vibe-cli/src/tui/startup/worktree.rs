//! The `Arguments`-facing half of the worktree contract.
//!
//! Preparation, inspection and removal live in `vibe_core::worktree`, one layer
//! below, so the app-server can reach them too. What is left here is what only
//! a terminal launch knows: which directory the invocation meant, where the
//! vibe home is, how `--add-dir` resolves once the effective directory moved,
//! and whether a human at a terminal agreed to discard their work.

use std::fs;
use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

use vibe_core::worktree::{
    self, PreparedWorktree, WorktreeError, inspect_worktree_for_cleanup, remove_worktree,
};

use crate::Arguments;

use super::{StartupError, startup_io, vibe_home_directory};

impl From<WorktreeError> for StartupError {
    fn from(error: WorktreeError) -> Self {
        match error {
            WorktreeError::InvalidName => Self::InvalidWorktreeName,
            WorktreeError::InvalidBranch { branch } => Self::InvalidWorktreeBranch { branch },
            WorktreeError::RepositoryRequired => Self::WorktreeRepositoryRequired,
            WorktreeError::GitUnavailable(message) => Self::WorktreeGitUnavailable(message),
            WorktreeError::Failed { name, message } => Self::Worktree { name, message },
            WorktreeError::Io { path, source } => Self::Io { path, source },
            WorktreeError::Noted { name, source, note } => Self::Worktree {
                name,
                message: format!("{source}; {note}"),
            },
        }
    }
}

/// Where a launch actually runs, and the worktree it prepared to get there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchWorkspace {
    pub effective_directory: PathBuf,
    pub worktree: Option<PreparedWorktree>,
}

impl LaunchWorkspace {
    pub fn prepare(arguments: &mut Arguments) -> Result<Self, StartupError> {
        let requested = match &arguments.workdir {
            Some(path) => path.clone(),
            None => std::env::current_dir().map_err(|source| StartupError::Io {
                path: PathBuf::from("."),
                source,
            })?,
        };
        let base_directory = canonical_directory(&expand_user_path(&requested))?;

        if arguments.worktree.is_none() || arguments.setup || arguments.check_upgrade {
            resolve_additional_directories(arguments, &base_directory)?;
            arguments.workdir = Some(base_directory.clone());
            return Ok(Self {
                effective_directory: base_directory,
                worktree: None,
            });
        }

        let name = arguments.worktree.as_deref().unwrap_or_default();
        let vibe_home = vibe_home_directory(arguments, &base_directory);
        let worktree = worktree::prepare_worktree(name, &base_directory, &vibe_home)?;
        resolve_additional_directories(arguments, &worktree.path)?;
        arguments.workdir = Some(worktree.path.clone());
        arguments.trust = true;
        Ok(Self {
            effective_directory: worktree.path.clone(),
            worktree: Some(worktree),
        })
    }
}

fn resolve_additional_directories(
    arguments: &mut Arguments,
    effective_directory: &Path,
) -> Result<(), StartupError> {
    for directory in &mut arguments.add_directories {
        let expanded = expand_user_path(directory);
        let candidate = if expanded.is_absolute() {
            expanded
        } else {
            effective_directory.join(expanded)
        };
        *directory = canonical_directory(&candidate)?;
    }
    Ok(())
}

fn expand_user_path(path: &Path) -> PathBuf {
    let Some(text) = path.to_str() else {
        return path.to_path_buf();
    };
    if text == "~" {
        return std::env::var_os("HOME").map_or_else(|| path.to_path_buf(), PathBuf::from);
    }
    for prefix in ["~/", "~\\"] {
        if let Some(remainder) = text.strip_prefix(prefix)
            && let Some(home) = std::env::var_os("HOME")
        {
            return PathBuf::from(home).join(remainder);
        }
    }
    path.to_path_buf()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CleanupOutcome {
    NotOwned,
    Kept,
    Removed,
}

/// Offers cleanup on the terminal, reading `/dev/tty` when stdin is a pipe.
pub fn cleanup_worktree_terminal(
    worktree: PreparedWorktree,
) -> Result<CleanupOutcome, StartupError> {
    let mut output = std::io::stderr().lock();
    if std::io::stdin().is_terminal() {
        return cleanup_worktree(worktree, &mut std::io::stdin().lock(), &mut output);
    }
    #[cfg(unix)]
    {
        let terminal = fs::File::open("/dev/tty")
            .map_err(|source| startup_io(Path::new("/dev/tty"), source))?;
        cleanup_worktree(
            worktree,
            &mut std::io::BufReader::new(terminal),
            &mut output,
        )
    }
    #[cfg(not(unix))]
    cleanup_worktree(worktree, &mut std::io::stdin().lock(), &mut output)
}

/// Asks whether to discard the worktree, then removes it through the core.
pub fn cleanup_worktree(
    worktree: PreparedWorktree,
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> Result<CleanupOutcome, StartupError> {
    if !worktree.created {
        return Ok(CleanupOutcome::NotOwned);
    }
    let state = inspect_worktree_for_cleanup(&worktree)?;
    if !state.is_clean() {
        writeln!(
            output,
            "Worktree {:?} has {}.",
            worktree.name,
            state.reasons().join(", ")
        )
        .map_err(|source| startup_io(&worktree.root, source))?;
        writeln!(
            output,
            "Remove it and delete its branch? This discards worktree changes, untracked files, and commits."
        )
        .map_err(|source| startup_io(&worktree.root, source))?;
        if !confirm(
            input,
            output,
            "Remove worktree? [y/N] ",
            &["y", "yes", "remove"],
        )? {
            writeln!(output, "Keeping worktree: {}", worktree.root.display())
                .map_err(|source| startup_io(&worktree.root, source))?;
            return Ok(CleanupOutcome::Kept);
        }
    }

    let delete_branch = if worktree.branch_created {
        true
    } else {
        writeln!(
            output,
            "Branch {:?} existed before this session and was attached, not created by Vibe.",
            worktree.branch
        )
        .map_err(|source| startup_io(&worktree.root, source))?;
        confirm(
            input,
            output,
            &format!("Also delete branch {:?}? [y/N] ", worktree.branch),
            &["y", "yes", "delete"],
        )?
    };

    remove_worktree(&worktree, delete_branch)?;
    writeln!(output, "Removed worktree: {}", worktree.root.display())
        .map_err(|source| startup_io(&worktree.root, source))?;
    if !delete_branch {
        writeln!(output, "Kept branch: {}", worktree.branch)
            .map_err(|source| startup_io(&worktree.root, source))?;
    }
    Ok(CleanupOutcome::Removed)
}

fn confirm(
    input: &mut impl BufRead,
    output: &mut impl Write,
    prompt: &str,
    accepted: &[&str],
) -> Result<bool, StartupError> {
    write!(output, "{prompt}").map_err(|source| startup_io(Path::new("stderr"), source))?;
    output
        .flush()
        .map_err(|source| startup_io(Path::new("stderr"), source))?;
    let mut answer = String::new();
    let read = input
        .read_line(&mut answer)
        .map_err(|source| startup_io(Path::new("stdin"), source))?;
    Ok(read > 0 && accepted.contains(&answer.trim().to_ascii_lowercase().as_str()))
}

fn canonical_directory(path: &Path) -> Result<PathBuf, StartupError> {
    let canonical = fs::canonicalize(path).map_err(|source| startup_io(path, source))?;
    if canonical.is_dir() {
        Ok(canonical)
    } else {
        Err(StartupError::Io {
            path: path.to_path_buf(),
            source: std::io::Error::new(
                std::io::ErrorKind::NotADirectory,
                "path is not a directory",
            ),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::process::Command;

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
        arguments.worktree = Some("parity".to_owned());
        let first = LaunchWorkspace::prepare(&mut arguments).expect("worktree prepared");
        let prepared = first.worktree.expect("prepared worktree");
        assert!(prepared.created);
        assert!(prepared.path.join("README.md").is_file());
        assert_eq!(arguments.workdir.as_deref(), Some(prepared.path.as_path()));
        assert!(arguments.trust);

        let mut reused_arguments = test_arguments(root.path());
        reused_arguments.worktree = Some("parity".to_owned());
        let reused = LaunchWorkspace::prepare(&mut reused_arguments)
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

    #[test]
    fn invalid_and_non_repository_worktrees_fail_before_checkout_changes() {
        let root = repository();
        let original_head = git_stdout(root.path(), &["rev-parse", "HEAD"]);
        let mut invalid = test_arguments(root.path());
        invalid.worktree = Some("../escape".to_owned());
        assert!(matches!(
            LaunchWorkspace::prepare(&mut invalid),
            Err(StartupError::InvalidWorktreeName)
        ));
        assert_eq!(
            git_stdout(root.path(), &["rev-parse", "HEAD"]),
            original_head
        );

        let outside = tempfile::tempdir().expect("non-repository");
        let mut non_repository = test_arguments(outside.path());
        non_repository.worktree = Some("feature".to_owned());
        assert!(matches!(
            LaunchWorkspace::prepare(&mut non_repository),
            Err(StartupError::WorktreeRepositoryRequired)
        ));
    }

    #[test]
    fn conflicting_worktree_path_is_preserved() {
        let root = repository();
        let mut arguments = test_arguments(root.path());
        arguments.worktree = Some("conflict".to_owned());
        let prepared = LaunchWorkspace::prepare(&mut arguments)
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
        conflicting.worktree = Some("conflict".to_owned());
        assert!(matches!(
            LaunchWorkspace::prepare(&mut conflicting),
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
        arguments.worktree = Some("attached".to_owned());
        let prepared = LaunchWorkspace::prepare(&mut arguments)
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
        arguments.worktree = Some("relative-add-dir".to_owned());
        arguments.add_directories = vec![PathBuf::from("nested/context")];
        let prepared = LaunchWorkspace::prepare(&mut arguments)
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
        arguments.worktree = Some("dirty".to_owned());
        let prepared = LaunchWorkspace::prepare(&mut arguments)
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
        arguments.worktree = Some("detached".to_owned());
        let prepared = LaunchWorkspace::prepare(&mut arguments)
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

    #[test]
    fn clean_owned_worktree_is_removed_once() {
        let root = repository();
        let mut arguments = test_arguments(root.path());
        arguments.worktree = Some("clean".to_owned());
        let prepared = LaunchWorkspace::prepare(&mut arguments)
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
}
