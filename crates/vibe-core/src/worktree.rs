//! Managed git worktrees: preparation, inspection and removal.
//!
//! The reference publishes this contract from `vibe/core/worktree.py`, one
//! layer below its CLI, and three of its consumers import it from there. This
//! port had it in `vibe-cli`, which put it out of reach of the app-server and
//! made every function speak in terms of parsed command-line arguments. It
//! lives here for the same reason the reference's does: a session lifecycle is
//! not a terminal concern.
//!
//! Everything a terminal owns stays in the CLI adapter: prompting before a
//! destructive removal, reading `/dev/tty`, and resolving `--add-dir`. What is
//! here is the part that answers to git and to the managed root.
//!
//! `crates/vibe-core/src/worktree/worktree_parity_tests.rs` replays the
//! reference's own verdicts over the same scripted repositories, so a
//! divergence in this file fails the build instead of aging into a wrong
//! scorecard row.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use sha2::{Digest, Sha256};
use thiserror::Error;

#[cfg(test)]
mod worktree_parity_tests;
#[cfg(test)]
mod worktree_tests;

/// The directory the managed roots live under, inside the vibe home.
const MANAGED_DIRECTORY: &str = "worktrees";

/// How many hex digits of the common git directory's digest name a managed
/// root. The reference takes the same twelve (`vibe/core/worktree.py:349`).
const REPOSITORY_DIGEST_LENGTH: usize = 12;

/// A worktree a session prepared, and everything cleanup needs to judge it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedWorktree {
    pub name: String,
    pub branch: String,
    pub root: PathBuf,
    pub path: PathBuf,
    pub repo_root: PathBuf,
    pub base_commit: String,
    pub created: bool,
    pub branch_created: bool,
}

/// What a prepared worktree holds that removing it would destroy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeCleanupState {
    pub has_uncommitted_changes: bool,
    pub has_untracked_files: bool,
    pub new_commit_count: u64,
}

impl WorktreeCleanupState {
    #[must_use]
    pub const fn is_clean(&self) -> bool {
        !self.has_uncommitted_changes && !self.has_untracked_files && self.new_commit_count == 0
    }

    #[must_use]
    pub fn reasons(&self) -> Vec<String> {
        let mut reasons = Vec::new();
        if self.has_uncommitted_changes {
            reasons.push("uncommitted changes".to_owned());
        }
        if self.has_untracked_files {
            reasons.push("untracked files".to_owned());
        }
        if self.new_commit_count > 0 {
            let noun = if self.new_commit_count == 1 {
                "commit"
            } else {
                "commits"
            };
            reasons.push(format!(
                "{} {noun} added to the branch during this session",
                self.new_commit_count
            ));
        }
        reasons
    }
}

/// Every way a worktree operation can refuse.
///
/// One enum rather than a family, because the reference publishes one exception
/// hierarchy and the two cases callers actually discriminate are "this is not a
/// repository" and "git is not installed".
#[derive(Debug, Error)]
pub enum WorktreeError {
    #[error("--worktree NAME must be a single path segment")]
    InvalidName,
    #[error("--worktree requires a git repository")]
    RepositoryRequired,
    #[error("git worktree operations require git on PATH: {0}")]
    GitUnavailable(String),
    #[error("worktree `{name}` failed: {message}")]
    Failed { name: String, message: String },
    /// A failure that carries a second failure it did not replace.
    ///
    /// The reference attaches the rollback's own failure to the original
    /// exception with `add_note` rather than raising it
    /// (`vibe/core/worktree.py:210-220`), so the caller still learns why
    /// preparation failed and additionally learns that the cleanup after it
    /// did not finish. The class of the original is what the note is attached
    /// to, which is why the parity replay's `error_class` reads through this
    /// variant.
    #[error("worktree `{name}` failed: {source}; {note}")]
    Noted {
        name: String,
        #[source]
        source: Box<WorktreeError>,
        note: String,
    },
    #[error("worktree I/O failed at `{path}`: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

impl WorktreeError {
    fn failed(name: &str, message: impl Into<String>) -> Self {
        Self::Failed {
            name: name.to_owned(),
            message: message.into(),
        }
    }

    fn io(path: &Path, source: std::io::Error) -> Self {
        Self::Io {
            path: path.to_path_buf(),
            source,
        }
    }
}

/// Prepares the managed worktree named `name` for the checkout holding `base`.
///
/// `vibe_home` is a parameter rather than something resolved here, because the
/// managed root is a property of the caller's session, not of this module: the
/// CLI reads it from `--session-root` and `VIBE_HOME`, and the app-server has
/// its own workspace paths.
pub fn prepare_worktree(
    name: &str,
    base: &Path,
    vibe_home: &Path,
) -> Result<PreparedWorktree, WorktreeError> {
    validate_worktree_name(name)?;
    let checkout_root = git_stdout(base, ["rev-parse", "--show-toplevel"], name)
        .map(PathBuf::from)
        .map_err(|error| match error {
            WorktreeError::GitUnavailable(_) => error,
            _ => WorktreeError::RepositoryRequired,
        })?;
    let checkout_root = canonical_directory(&checkout_root)?;
    let common_git_dir = resolve_git_path(
        &checkout_root,
        &git_stdout(base, ["rev-parse", "--git-common-dir"], name)?,
    )?;
    let repo_root = common_git_dir
        .parent()
        .ok_or_else(|| {
            WorktreeError::failed(name, "git common directory has no repository parent")
        })?
        .to_path_buf();
    let relative_base = base.strip_prefix(&checkout_root).map_err(|_| {
        WorktreeError::failed(name, "working directory is outside the Git checkout")
    })?;
    let target = managed_worktree_root(vibe_home, &repo_root, &common_git_dir).join(name);

    if target.is_dir() {
        validate_existing_worktree(&target, name, &common_git_dir)?;
        return build_prepared_worktree(name, target, relative_base, repo_root, false, false);
    }

    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).map_err(|source| WorktreeError::io(parent, source))?;
    }
    let branch_exists = git_status(
        &checkout_root,
        [
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{name}"),
        ],
        name,
    )?;
    let target_text = path_text(&target)?;
    if branch_exists {
        git_checked(&checkout_root, ["worktree", "add", target_text, name], name)?;
    } else {
        git_checked(
            &checkout_root,
            ["worktree", "add", "-b", name, target_text],
            name,
        )?;
    }
    // Past this point the checkout on disk is this call's doing, so a failure
    // that follows has to undo it. The reference wraps exactly this span
    // (`vibe/core/worktree.py:140-153`) and everything above it fails before
    // anything exists to roll back.
    build_prepared_worktree(
        name,
        target.clone(),
        relative_base,
        repo_root,
        true,
        !branch_exists,
    )
    .map_err(|error| {
        match cleanup_failed_prepare(&checkout_root, &target, name, !branch_exists) {
            Some(note) => WorktreeError::Noted {
                name: name.to_owned(),
                source: Box::new(error),
                note,
            },
            None => error,
        }
    })
}

/// Undoes a worktree this call created, after preparation failed on it.
///
/// Returns the note to attach to the original failure when the rollback itself
/// failed, and [`None`] when it finished. The original error is never replaced:
/// the reference states that rule by calling `add_note` rather than raising
/// (`vibe/core/worktree.py:210-220`), and the caller who has to act on the
/// failure needs the first one, not the second.
fn cleanup_failed_prepare(
    checkout_root: &Path,
    target: &Path,
    name: &str,
    branch_created: bool,
) -> Option<String> {
    let target_text = path_text(target).ok()?;
    if let Err(error) = git_checked(
        checkout_root,
        ["worktree", "remove", "--force", target_text],
        name,
    ) {
        return Some(format!(
            "the worktree it created could not be removed: {error}"
        ));
    }
    if branch_created && let Err(error) = git_checked(checkout_root, ["branch", "-D", name], name) {
        return Some(format!(
            "the branch it created could not be deleted: {error}"
        ));
    }
    None
}

/// What removing this worktree would discard, relative to the session start.
///
/// The commit count is taken against the worktree's own `HEAD` rather than
/// against its named branch, which is what the reference does and says why:
/// a commit made while `HEAD` is detached never moves the branch tip, so
/// counting against the branch would report zero and let a removal discard it
/// (`vibe/core/worktree.py:263-267`).
pub fn inspect_worktree_for_cleanup(
    worktree: &PreparedWorktree,
) -> Result<WorktreeCleanupState, WorktreeError> {
    let status = git_stdout(
        &worktree.root,
        ["status", "--porcelain", "--untracked-files=all"],
        &worktree.name,
    )?;
    let range = format!("{}..HEAD", worktree.base_commit);
    let new_commit_count = git_stdout(
        &worktree.root,
        ["rev-list", "--count", range.as_str()],
        &worktree.name,
    )?
    .parse::<u64>()
    .map_err(|error| {
        WorktreeError::failed(&worktree.name, format!("invalid commit count: {error}"))
    })?;
    Ok(WorktreeCleanupState {
        has_uncommitted_changes: status.lines().any(|line| !line.starts_with("??")),
        has_untracked_files: status.lines().any(|line| line.starts_with("??")),
        new_commit_count,
    })
}

/// Removes a prepared worktree, and its branch when the caller says so.
///
/// `delete_branch` is the caller's decision because only the caller knows
/// whether the branch predates the session; `PreparedWorktree::branch_created`
/// is what the CLI answers it from.
pub fn remove_worktree(
    worktree: &PreparedWorktree,
    delete_branch: bool,
) -> Result<(), WorktreeError> {
    git_checked(
        &worktree.repo_root,
        ["worktree", "remove", "--force", path_text(&worktree.root)?],
        &worktree.name,
    )?;
    if delete_branch {
        git_checked(
            &worktree.repo_root,
            ["branch", "-D", worktree.branch.as_str()],
            &worktree.name,
        )?;
    }
    Ok(())
}

/// The managed root a checkout's worktrees live under.
///
/// The directory name is the repository's own name followed by twelve hex
/// digits of the common git directory's digest, which is what keeps two
/// checkouts of the same name apart under one vibe home.
fn managed_worktree_root(vibe_home: &Path, repo_root: &Path, common_git_dir: &Path) -> PathBuf {
    let digest = hex::encode(Sha256::digest(common_git_dir.to_string_lossy().as_bytes()));
    let repository_name = repo_root
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("repository");
    vibe_home.join(MANAGED_DIRECTORY).join(format!(
        "{repository_name}-{}",
        &digest[..REPOSITORY_DIGEST_LENGTH]
    ))
}

fn build_prepared_worktree(
    name: &str,
    root: PathBuf,
    relative_base: &Path,
    repo_root: PathBuf,
    created: bool,
    branch_created: bool,
) -> Result<PreparedWorktree, WorktreeError> {
    let path = target_cwd(&root, relative_base, name)?;
    let base_commit = git_stdout(&root, ["rev-parse", "HEAD"], name)?;
    Ok(PreparedWorktree {
        name: name.to_owned(),
        branch: name.to_owned(),
        root,
        path,
        repo_root,
        base_commit,
        created,
        branch_created,
    })
}

/// The directory a session runs in once the worktree exists.
fn target_cwd(root: &Path, relative_base: &Path, name: &str) -> Result<PathBuf, WorktreeError> {
    let path = root.join(relative_base);
    if path.is_dir() {
        Ok(path)
    } else {
        Err(WorktreeError::failed(
            name,
            format!(
                "worktree path `{}` does not exist after checkout",
                path.display()
            ),
        ))
    }
}

fn validate_existing_worktree(
    target: &Path,
    expected_branch: &str,
    expected_common_git_dir: &Path,
) -> Result<(), WorktreeError> {
    if !target.join(".git").is_file() {
        return Err(WorktreeError::failed(
            expected_branch,
            format!(
                "path `{}` already exists but is not a Git worktree",
                target.display()
            ),
        ));
    }
    let actual_common = resolve_git_path(
        target,
        &git_stdout(target, ["rev-parse", "--git-common-dir"], expected_branch)?,
    )?;
    if actual_common != expected_common_git_dir {
        return Err(WorktreeError::failed(
            expected_branch,
            format!(
                "path `{}` belongs to a different Git repository",
                target.display()
            ),
        ));
    }
    let actual_branch = git_stdout(target, ["branch", "--show-current"], expected_branch)?;
    if actual_branch != expected_branch {
        let actual = if actual_branch.is_empty() {
            "detached HEAD"
        } else {
            actual_branch.as_str()
        };
        return Err(WorktreeError::failed(
            expected_branch,
            format!(
                "path `{}` is checked out on `{actual}`, expected `{expected_branch}`",
                target.display()
            ),
        ));
    }
    Ok(())
}

fn validate_worktree_name(name: &str) -> Result<(), WorktreeError> {
    let is_single_segment = !name.is_empty()
        && !matches!(name, "." | "..")
        && Path::new(name).components().count() == 1
        && !name.contains(['/', '\\'])
        && !name
            .as_bytes()
            .get(1)
            .is_some_and(|character| *character == b':');
    if is_single_segment {
        Ok(())
    } else {
        Err(WorktreeError::InvalidName)
    }
}

fn resolve_git_path(base: &Path, value: &str) -> Result<PathBuf, WorktreeError> {
    let path = PathBuf::from(value);
    let path = if path.is_absolute() {
        path
    } else {
        base.join(path)
    };
    fs::canonicalize(&path).map_err(|source| WorktreeError::io(&path, source))
}

fn git_stdout<'a>(
    directory: &Path,
    arguments: impl IntoIterator<Item = &'a str>,
    name: &str,
) -> Result<String, WorktreeError> {
    let output = git_output(directory, arguments, name)?;
    if !output.status.success() {
        return Err(git_failure(name, &output));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn git_checked<'a>(
    directory: &Path,
    arguments: impl IntoIterator<Item = &'a str>,
    name: &str,
) -> Result<(), WorktreeError> {
    let output = git_output(directory, arguments, name)?;
    if output.status.success() {
        Ok(())
    } else {
        Err(git_failure(name, &output))
    }
}

fn git_status<'a>(
    directory: &Path,
    arguments: impl IntoIterator<Item = &'a str>,
    name: &str,
) -> Result<bool, WorktreeError> {
    git_output(directory, arguments, name).map(|output| output.status.success())
}

fn git_output<'a>(
    directory: &Path,
    arguments: impl IntoIterator<Item = &'a str>,
    name: &str,
) -> Result<Output, WorktreeError> {
    Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(arguments)
        .output()
        .map_err(|error| {
            // A missing binary is not a failure of this worktree; it is a
            // machine that cannot do worktrees at all, and the reference draws
            // the same line (`vibe/core/worktree.py:45-58`).
            if error.kind() == std::io::ErrorKind::NotFound {
                WorktreeError::GitUnavailable(error.to_string())
            } else {
                WorktreeError::failed(name, error.to_string())
            }
        })
}

fn git_failure(name: &str, output: &Output) -> WorktreeError {
    let message = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    WorktreeError::failed(
        name,
        if message.is_empty() {
            format!("git exited with {}", output.status)
        } else {
            message
        },
    )
}

fn canonical_directory(path: &Path) -> Result<PathBuf, WorktreeError> {
    let canonical = fs::canonicalize(path).map_err(|source| WorktreeError::io(path, source))?;
    if canonical.is_dir() {
        Ok(canonical)
    } else {
        Err(WorktreeError::io(
            path,
            std::io::Error::new(std::io::ErrorKind::NotADirectory, "path is not a directory"),
        ))
    }
}

fn path_text(path: &Path) -> Result<&str, WorktreeError> {
    path.to_str().ok_or_else(|| {
        WorktreeError::failed(
            "path",
            format!("path `{}` is not valid UTF-8", path.display()),
        )
    })
}
