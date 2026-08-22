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
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output};
use std::sync::LazyLock;

use regex::Regex;
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

/// The characters a Windows path cannot carry, which is the set the reference
/// refuses a worktree name for (`vibe/core/worktree.py:19`).
const INVALID_NAME_CHARACTERS: [char; 9] = ['<', '>', ':', '"', '/', '\\', '|', '?', '*'];

/// The device names Windows reserves whatever the extension, upper-cased for
/// comparison. The reference builds the same set at `vibe/core/worktree.py:21`.
const RESERVED_DEVICE_NAMES: [&str; 25] = [
    "CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$", "CLOCK$", "COM1", "COM2", "COM3", "COM4",
    "COM5", "COM6", "COM7", "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7",
    "LPT8", "LPT9",
];

/// The status git exits with when it refuses an option it does not know.
///
/// `git worktree list` learned `-z` in 2.36, and an older git answers this
/// instead of a listing. It is the one status enumeration retries without the
/// flag on; anything else is a real failure. The reference watches the same
/// number (`vibe/core/worktree.py:464-476`).
const GIT_USAGE_ERROR_STATUS: i32 = 129;

/// What names an enumeration in a git failure, since it runs for no one
/// worktree.
const ENUMERATION: &str = "list";

/// What Windows canonicalization prefixes an absolute path with.
const VERBATIM_PREFIX: &str = r"\\?\";

/// The `\\?\UNC\` form, which names a network share rather than a drive.
const VERBATIM_UNC_PREFIX: &str = r"\\?\UNC\";

/// One character Python calls non-printable: anything in the Unicode `Other`
/// or `Separator` categories, minus the ASCII space.
///
/// `str.isprintable` is what the reference tests a name with
/// (`vibe/core/worktree.py:311`) and the standard library exposes no general
/// category, so the classes come from `regex`, which vibe-core already
/// depends on and which carries the Unicode tables the check needs.
#[expect(
    clippy::expect_used,
    reason = "the pattern is a compile-time constant that compiles"
)]
static NON_PRINTABLE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[^\P{C} ]|[^\P{Z} ]").expect("the non-printable pattern compiles")
});

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

/// A linked worktree of a checkout, as enumeration reports it.
///
/// Deliberately not a [`PreparedWorktree`]: nothing here was created by this
/// process, so there is no base commit to measure against and no decision about
/// whether removing it may delete a branch. The reference keeps the same two
/// records apart for the same reason (`vibe/core/worktree.py:61-77`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkedWorktree {
    pub name: String,
    pub branch: String,
    pub root: PathBuf,
    pub path: PathBuf,
    pub repo_root: PathBuf,
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

    /// Why removing this worktree would lose work, in the reference's order and
    /// its wording: the two booleans first, then the commit count with the noun
    /// agreeing with it (`vibe/core/worktree.py:94-104`).
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
                "{} {noun} added during this session",
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
    #[error(
        "--worktree NAME must be one portable path segment: no separator, no drive letter, \
             no character a Windows path forbids, and no reserved device name"
    )]
    InvalidName,
    /// A branch git itself refuses to name a ref with.
    ///
    /// Separate from [`Self::InvalidName`] because the two are asked at
    /// different times of different strings: the reference validates the name
    /// against a portability rule of its own and hands the branch to
    /// `git check-ref-format --branch` (`vibe/core/worktree.py:320-325`), and a
    /// branch is allowed to differ from the worktree it belongs to.
    #[error("--worktree branch `{branch}` is not a valid Git branch name")]
    InvalidBranch { branch: String },
    /// A managed root that resolved out of the vibe home it was built under.
    #[error("managed worktree root `{target}` resolves outside `{managed_root}`")]
    ManagedRootEscape {
        target: PathBuf,
        managed_root: PathBuf,
    },
    #[error("--worktree requires a git repository")]
    RepositoryRequired,
    #[error("git worktree operations require git on PATH: {0}")]
    GitUnavailable(String),
    #[error("worktree `{name}` failed: {message}")]
    Failed { name: String, message: String },
    /// Git refused to enumerate the worktrees of a checkout that exists.
    ///
    /// Separate from [`Self::Failed`] because enumeration answers for no single
    /// worktree, so there is no name to blame. The reference raises the base
    /// exception with the same phrasing (`vibe/core/worktree.py:464-476`).
    #[error("failed to list git worktrees: {message}")]
    ListFailed { message: String },
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
/// `branch` is what the worktree is checked out on, and [`None`] means the
/// worktree's own name. The two are separate because a client that picks a
/// directory name and a branch name independently has to be able to say so: the
/// reference takes the same optional parameter and defaults it the same way
/// (`vibe/core/worktree.py:108-120`).
///
/// `vibe_home` is a parameter rather than something resolved here, because the
/// managed root is a property of the caller's session, not of this module: the
/// CLI reads it from `--session-root` and `VIBE_HOME`, and the app-server has
/// its own workspace paths.
pub fn prepare_worktree(
    name: &str,
    base: &Path,
    vibe_home: &Path,
    branch: Option<&str>,
) -> Result<PreparedWorktree, WorktreeError> {
    validate_worktree_name(name)?;
    let checkout_root = open_checkout(base, name)?;
    // The branch is validated before anything exists, which is the whole point
    // of the gate: the reference asks git the question between opening the
    // repository and reaching for the managed root
    // (`vibe/core/worktree.py:114-120`), so a refusal leaves no directory and
    // no branch behind.
    let branch = branch.unwrap_or(name);
    validate_branch_name(&checkout_root, branch)?;
    // Both rev-parse answers are read from the checkout root rather than from
    // `base`, because git reports them relative to the directory it ran in and
    // from a subdirectory `--git-common-dir` answers `../.git`. The reference
    // reads them off a repository object anchored at its working directory
    // (`vibe/core/worktree.py:340-346`), which is the same anchor.
    let (git_dir, common_git_dir) = checkout_git_directories(&checkout_root, name)?;
    let repo_root = primary_worktree_root(&checkout_root, &git_dir, &common_git_dir, name)?;
    let relative_base = relative_base(&checkout_root, base, name)?;
    let relative_base = relative_base.as_path();
    let target = managed_worktree_root(vibe_home, &repo_root, &common_git_dir)?.join(name);

    if target.is_dir() {
        validate_existing_worktree(&target, branch, &common_git_dir)?;
        return build_prepared_worktree(
            name,
            branch,
            target,
            relative_base,
            repo_root,
            false,
            false,
        );
    }

    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).map_err(|source| WorktreeError::io(parent, source))?;
    }
    let branch_exists = branch_exists(&checkout_root, branch, name)?;
    let target_text = path_text(&target)?;
    if branch_exists {
        git_checked(
            &checkout_root,
            ["worktree", "add", target_text, branch],
            name,
        )?;
    } else {
        git_checked(
            &checkout_root,
            ["worktree", "add", "-b", branch, target_text],
            name,
        )?;
    }
    // Past this point the checkout on disk is this call's doing, so a failure
    // that follows has to undo it. The reference wraps exactly this span
    // (`vibe/core/worktree.py:140-153`) and everything above it fails before
    // anything exists to roll back.
    build_prepared_worktree(
        name,
        branch,
        target.clone(),
        relative_base,
        repo_root,
        true,
        !branch_exists,
    )
    .map_err(|error| {
        match cleanup_failed_prepare(
            &checkout_root,
            &target,
            name,
            (!branch_exists).then_some(branch),
        ) {
            Some(note) => WorktreeError::Noted {
                name: name.to_owned(),
                source: Box::new(error),
                note,
            },
            None => error,
        }
    })
}

/// Every linked worktree of the checkout holding `base`, in path order.
///
/// The primary checkout is not one of them: git reports it first and this drops
/// it, which is what makes the answer a list of *linked* worktrees. A record git
/// marks prunable, one on a detached `HEAD`, and one this port would refuse to
/// reuse are all dropped rather than raised, because a stale entry in another
/// checkout is not a reason to refuse to answer at all. The reference draws the
/// same four exclusions (`vibe/core/worktree.py:156-183`).
///
/// `path` is where a session would open inside each worktree: `base`'s own
/// position in the checkout, carried across. Asking from a subdirectory
/// therefore answers with the matching subdirectory of every worktree.
pub fn list_linked_worktrees(base: &Path) -> Result<Vec<LinkedWorktree>, WorktreeError> {
    let checkout_root = open_checkout(base, ENUMERATION)?;
    let (git_dir, common_git_dir) = checkout_git_directories(&checkout_root, ENUMERATION)?;
    let records = worktree_records(&checkout_root)?;
    let repo_root = primary_worktree_root(&checkout_root, &git_dir, &common_git_dir, ENUMERATION)?;
    let relative_base = relative_base(&checkout_root, base, ENUMERATION)?;

    let mut linked = Vec::new();
    for record in records.into_iter().skip(1) {
        let (Some(branch), false) = (record.branch, record.prunable) else {
            continue;
        };
        if validate_existing_worktree(&record.root, &branch, &common_git_dir).is_err() {
            continue;
        }
        let root = resolve_lenient(&record.root);
        let Ok(path) = target_cwd(&root, &relative_base, ENUMERATION) else {
            continue;
        };
        let name = root
            .file_name()
            .map(|value| value.to_string_lossy().into_owned())
            .unwrap_or_default();
        linked.push(LinkedWorktree {
            name,
            branch,
            root,
            path,
            repo_root: repo_root.clone(),
        });
    }
    // Ordered by the string form of the path rather than by the path itself,
    // because that is the key the reference sorts on and the two disagree on
    // any platform whose path comparison is not byte-wise.
    linked.sort_by(|left, right| {
        left.path
            .to_string_lossy()
            .cmp(&right.path.to_string_lossy())
    });
    Ok(linked)
}

/// The working directory of the checkout holding `base`.
///
/// A directory that is not inside a repository is [`WorktreeError::RepositoryRequired`]
/// rather than whatever git said, which is the distinction every caller acts
/// on; a machine without git keeps its own answer.
fn open_checkout(base: &Path, name: &str) -> Result<PathBuf, WorktreeError> {
    let root = git_stdout(base, ["rev-parse", "--show-toplevel"], name)
        .map(PathBuf::from)
        .map_err(|error| match error {
            WorktreeError::GitUnavailable(_) => error,
            _ => WorktreeError::RepositoryRequired,
        })?;
    canonical_directory(&root)
}

/// The checkout's own git directory and the one it shares with its siblings.
///
/// Both are read from the checkout root rather than from wherever the call
/// started, because git reports them relative to the directory it ran in and
/// from a subdirectory `--git-common-dir` answers `../.git`. The reference reads
/// them off a repository object anchored at its working directory
/// (`vibe/core/worktree.py:340-346`), which is the same anchor.
fn checkout_git_directories(
    checkout_root: &Path,
    name: &str,
) -> Result<(PathBuf, PathBuf), WorktreeError> {
    let git_dir = resolve_git_path(
        checkout_root,
        &git_stdout(checkout_root, ["rev-parse", "--git-dir"], name)?,
    )?;
    let common_git_dir = resolve_git_path(
        checkout_root,
        &git_stdout(checkout_root, ["rev-parse", "--git-common-dir"], name)?,
    )?;
    Ok((git_dir, common_git_dir))
}

/// One entry of a porcelain worktree listing, reduced to what enumeration
/// reads.
#[derive(Debug, Clone, PartialEq, Eq)]
struct WorktreeRecord {
    root: PathBuf,
    branch: Option<String>,
    prunable: bool,
}

/// Asks git for the porcelain listing, in whichever of its two spellings this
/// git understands.
///
/// `-z` is tried first because a NUL-separated listing is the only one that
/// survives a path containing a newline. A git too old to know the flag answers
/// [`GIT_USAGE_ERROR_STATUS`] and is asked again without it; any other refusal
/// is reported. The reference makes the same two attempts
/// (`vibe/core/worktree.py:464-476`).
fn worktree_records(checkout_root: &Path) -> Result<Vec<WorktreeRecord>, WorktreeError> {
    let terminated = git_output(
        checkout_root,
        ["worktree", "list", "--porcelain", "-z"],
        ENUMERATION,
    )?;
    if terminated.status.success() {
        return Ok(parse_worktree_records(
            &String::from_utf8_lossy(&terminated.stdout),
            '\0',
        ));
    }
    if terminated.status.code() != Some(GIT_USAGE_ERROR_STATUS) {
        return Err(WorktreeError::ListFailed {
            message: git_message(&terminated),
        });
    }
    let plain = git_output(
        checkout_root,
        ["worktree", "list", "--porcelain"],
        ENUMERATION,
    )?;
    if !plain.status.success() {
        return Err(WorktreeError::ListFailed {
            message: git_message(&plain),
        });
    }
    Ok(parse_worktree_records(
        &String::from_utf8_lossy(&plain.stdout),
        '\n',
    ))
}

/// Splits a porcelain listing into records on `separator`.
///
/// An empty token ends the record being built, which is how both spellings
/// separate their entries: the NUL form writes a bare NUL and the plain form a
/// blank line. A `worktree` attribute also ends the previous record, so a
/// listing whose last entry is not terminated still yields it. Attributes this
/// does not name are skipped rather than refused, because git is free to add
/// them. The reference reads the same three (`vibe/core/worktree.py:486-513`).
fn parse_worktree_records(output: &str, separator: char) -> Vec<WorktreeRecord> {
    let mut records = Vec::new();
    let mut current: Option<WorktreeRecord> = None;
    for token in output.split(separator) {
        if token.is_empty() {
            records.extend(current.take());
            continue;
        }
        let (field, value) = token.split_once(' ').unwrap_or((token, ""));
        match field {
            "worktree" => {
                records.extend(current.take());
                current = Some(WorktreeRecord {
                    root: PathBuf::from(value),
                    branch: None,
                    prunable: false,
                });
            }
            "branch" => {
                if let Some(record) = current.as_mut() {
                    record.branch = Some(
                        value
                            .strip_prefix("refs/heads/")
                            .unwrap_or(value)
                            .to_owned(),
                    );
                }
            }
            "prunable" => {
                if let Some(record) = current.as_mut() {
                    record.prunable = true;
                }
            }
            _ => {}
        }
    }
    records.extend(current);
    records
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
    created_branch: Option<&str>,
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
    if let Some(branch) = created_branch
        && let Err(error) = git_checked(checkout_root, ["branch", "-D", branch], name)
    {
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
///
/// Both the managed directory and the repository's own root are resolved
/// before the second is checked against the first, so a vibe home reached
/// through a symbolic link names the same repository as the one reached
/// directly, and a repository directory that resolves out of the managed root
/// is refused rather than written to. The reference draws the same two steps
/// (`vibe/core/worktree.py:347-357`).
fn managed_worktree_root(
    vibe_home: &Path,
    repo_root: &Path,
    common_git_dir: &Path,
) -> Result<PathBuf, WorktreeError> {
    let digest = hex::encode(Sha256::digest(
        strip_verbatim_prefix(&common_git_dir.to_string_lossy()).as_bytes(),
    ));
    let repository_name = repo_root
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("repository");
    let managed_root = resolve_lenient(&vibe_home.join(MANAGED_DIRECTORY));
    let target = resolve_lenient(&managed_root.join(format!(
        "{repository_name}-{}",
        &digest[..REPOSITORY_DIGEST_LENGTH]
    )));
    if target.starts_with(&managed_root) {
        Ok(target)
    } else {
        Err(WorktreeError::ManagedRootEscape {
            target,
            managed_root,
        })
    }
}

/// Where `base` sits inside the checkout, as the worktree will spell it.
///
/// The base is resolved before it is measured, so a directory reached through
/// a symbolic link is placed by where it actually is rather than by how the
/// invocation spelled it. A base that resolves out of the checkout is refused
/// naming both paths, which is the reference's own refusal
/// (`vibe/core/worktree.py:360-367`).
fn relative_base(checkout_root: &Path, base: &Path, name: &str) -> Result<PathBuf, WorktreeError> {
    let resolved = resolve_lenient(base);
    resolved
        .strip_prefix(checkout_root)
        .map(Path::to_path_buf)
        .map_err(|_| {
            WorktreeError::failed(
                name,
                format!(
                    "path `{}` is outside Git worktree `{}`",
                    resolved.display(),
                    checkout_root.display()
                ),
            )
        })
}

/// The path string the digest is taken over, without what Windows
/// canonicalization prefixes it with.
///
/// The reference hashes `Path.resolve()`, which never carries the verbatim
/// prefix; `fs::canonicalize` on Windows always does, so hashing its output
/// unchanged would name a different directory for the same repository. On
/// every other platform this is the identity.
fn strip_verbatim_prefix(value: &str) -> String {
    if let Some(remainder) = value.strip_prefix(VERBATIM_UNC_PREFIX) {
        format!(r"\\{remainder}")
    } else if let Some(remainder) = value.strip_prefix(VERBATIM_PREFIX) {
        remainder.to_owned()
    } else {
        value.to_owned()
    }
}

/// The reference's non-strict `resolve` over a path that may not exist yet:
/// the deepest existing ancestor is canonicalized and the remainder appended,
/// so two spellings of one location compare equal.
fn resolve_lenient(path: &Path) -> PathBuf {
    if let Ok(resolved) = fs::canonicalize(path) {
        return resolved;
    }
    let mut remainder = Vec::new();
    let mut cursor = path;
    while let Some(parent) = cursor.parent() {
        if let Some(name) = cursor.file_name() {
            remainder.push(name.to_owned());
        }
        if let Ok(resolved) = fs::canonicalize(parent) {
            let mut resolved = resolved;
            for part in remainder.iter().rev() {
                resolved.push(part);
            }
            return resolved;
        }
        cursor = parent;
    }
    path.to_path_buf()
}

/// The working directory of the checkout every managed worktree hangs off.
///
/// Cleanup runs `git -C repo_root worktree remove`, so this has to be a real
/// checkout and not merely the directory beside the git data. A repository
/// created with `--separate-git-dir` reports that directory as its first
/// worktree, so the primary checkout's own working directory is the only
/// authoritative answer when the invocation is standing in it, and there is no
/// answer at all from a linked worktree of such a repository. The reference
/// draws the same three cases (`vibe/core/worktree.py:516-526`).
fn primary_worktree_root(
    checkout_root: &Path,
    git_dir: &Path,
    common_git_dir: &Path,
    name: &str,
) -> Result<PathBuf, WorktreeError> {
    if git_dir == common_git_dir {
        return Ok(checkout_root.to_path_buf());
    }
    if common_git_dir
        .file_name()
        .is_some_and(|value| value == ".git")
    {
        return common_git_dir
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| {
                WorktreeError::failed(name, "git common directory has no repository parent")
            });
    }
    Err(WorktreeError::failed(
        name,
        "the primary checkout cannot be determined from a linked worktree of a repository using \
         a separate git directory",
    ))
}

fn build_prepared_worktree(
    name: &str,
    branch: &str,
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
        branch: branch.to_owned(),
        root,
        path,
        repo_root,
        base_commit,
        created,
        branch_created,
    })
}

/// The directory a session runs in once the worktree exists.
///
/// Four questions, in the reference's own order
/// (`vibe/core/worktree.py:427-450`): the path resolves, it is a directory, it
/// still lies inside the worktree once resolved, and nothing between it and
/// the worktree root carries a `.git` entry. The third is what a symbolic link
/// planted in the checkout would otherwise walk through, and the fourth is
/// what keeps a session from opening inside a nested repository the worktree
/// merely contains. The worktree root itself is not asked the fourth question,
/// because its own `.git` file is what makes it a worktree.
fn target_cwd(root: &Path, relative_base: &Path, name: &str) -> Result<PathBuf, WorktreeError> {
    let root = resolve_lenient(root);
    let path = root.join(relative_base);
    let resolved = fs::canonicalize(&path).map_err(|_| {
        WorktreeError::failed(
            name,
            format!(
                "worktree path `{}` does not exist after checkout",
                path.display()
            ),
        )
    })?;
    if !resolved.is_dir() {
        return Err(WorktreeError::failed(
            name,
            format!("worktree path `{}` is not a directory", path.display()),
        ));
    }
    if !resolved.starts_with(&root) {
        return Err(WorktreeError::failed(
            name,
            format!(
                "worktree path `{}` resolves outside worktree `{}`",
                path.display(),
                root.display()
            ),
        ));
    }
    let mut current = resolved.clone();
    while current != root {
        let marker = current.join(".git");
        if marker.exists() || marker.is_symlink() {
            return Err(WorktreeError::failed(
                name,
                format!(
                    "worktree path `{}` belongs to a different Git repository",
                    resolved.display()
                ),
            ));
        }
        let Some(parent) = current.parent().map(Path::to_path_buf) else {
            break;
        };
        current = parent;
    }
    Ok(resolved)
}

/// Whether any component of `path` below the first one is a symbolic link.
///
/// The first component under the anchor is skipped: a root-level alias such as
/// macOS's `/tmp` belongs to the operating system rather than to the worktree
/// hierarchy, and refusing it would refuse every managed root on that
/// platform. The reference states the same exemption
/// (`vibe/core/worktree.py:415-418`).
///
/// On Windows the reference also asks whether a component is a junction. Rust
/// reports the reparse points its own `symlink_metadata` recognizes through
/// [`Path::is_symlink`], which is the closest this port can get without an
/// unsafe volume query.
fn has_linked_path_component(path: &Path) -> bool {
    let mut current = PathBuf::new();
    let mut anchored = false;
    let mut skipped_first = false;
    for component in path.components() {
        if matches!(component, Component::Prefix(_) | Component::RootDir) {
            anchored = true;
            current.push(component.as_os_str());
            continue;
        }
        current.push(component.as_os_str());
        if anchored && !skipped_first {
            skipped_first = true;
            continue;
        }
        if current.is_symlink() {
            return true;
        }
    }
    false
}

fn validate_existing_worktree(
    target: &Path,
    expected_branch: &str,
    expected_common_git_dir: &Path,
) -> Result<(), WorktreeError> {
    if has_linked_path_component(target) {
        return Err(WorktreeError::failed(
            expected_branch,
            format!(
                "path `{}` crosses a symbolic link, which is not a stable Git worktree path",
                target.display()
            ),
        ));
    }
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

/// Refuses a name no filesystem this port runs on can carry as one directory.
///
/// The rule is the reference's, question for question
/// (`vibe/core/worktree.py:305-317`): not empty and not a relative alias, no
/// trailing space or dot, printable, none of the characters a Windows path
/// forbids, no collision with a reserved device name whatever follows the
/// first dot, and one path segment. The name is refused before anything is
/// created, so a refusal never leaves a directory behind.
fn validate_worktree_name(name: &str) -> Result<(), WorktreeError> {
    if is_portable_worktree_name(name) {
        Ok(())
    } else {
        Err(WorktreeError::InvalidName)
    }
}

fn is_portable_worktree_name(name: &str) -> bool {
    if name.is_empty() || matches!(name, "." | "..") {
        return false;
    }
    if name.ends_with(' ') || name.ends_with('.') || NON_PRINTABLE.is_match(name) {
        return false;
    }
    if name.contains(INVALID_NAME_CHARACTERS) {
        return false;
    }
    // Windows reserves the device names whatever extension follows, so the
    // comparison is against what precedes the first dot.
    let device = name.split('.').next().unwrap_or(name).to_uppercase();
    if RESERVED_DEVICE_NAMES.contains(&device.as_str()) {
        return false;
    }
    // Every separator and drive marker either path spelling recognizes is
    // already refused above, so what is left for this to catch is a component
    // the parser folds away.
    Path::new(name).components().count() == 1
}

/// Asks git whether `branch` is a name it would accept for a ref.
///
/// `check-ref-format` reads no repository, so the directory only decides where
/// the process starts; a machine without git still reports
/// [`WorktreeError::GitUnavailable`] rather than calling the branch invalid.
/// The reference asks the same question through the same command
/// (`vibe/core/worktree.py:320-325`).
fn validate_branch_name(directory: &Path, branch: &str) -> Result<(), WorktreeError> {
    let output = git_output(directory, ["check-ref-format", "--branch", branch], branch)?;
    if output.status.success() {
        Ok(())
    } else {
        Err(WorktreeError::InvalidBranch {
            branch: branch.to_owned(),
        })
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

/// Whether `branch` is a local branch of the repository `directory` sits in.
///
/// `show-ref --verify --quiet` answers 1 for a ref that is not there and any
/// other non-zero status for a repository it could not read, and only the first
/// of those means "absent". Reporting the second as absent would take the
/// create-a-branch path on a repository git already refused to inspect, so it
/// is an error here exactly as it is upstream
/// (`vibe/core/worktree.py:327-335`).
fn branch_exists(directory: &Path, branch: &str, name: &str) -> Result<bool, WorktreeError> {
    let reference = format!("refs/heads/{branch}");
    let output = git_output(
        directory,
        ["show-ref", "--verify", "--quiet", reference.as_str()],
        name,
    )?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(WorktreeError::failed(
            name,
            format!(
                "failed to inspect worktree branch `{branch}`: {}",
                git_message(&output)
            ),
        )),
    }
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
    WorktreeError::failed(name, git_message(output))
}

/// What git said about a refusal: its own stderr, or its exit status when it
/// said nothing.
fn git_message(output: &Output) -> String {
    let message = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    if message.is_empty() {
        format!("git exited with {}", output.status)
    } else {
        message
    }
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
